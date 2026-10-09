use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use chrono::Utc;
use derive_builder::Builder;
use derive_getters::Getters;
use futures::StreamExt;
use log::{debug, error, info, warn};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{fs, sync::Notify};
use tokio_util::sync::CancellationToken;

use crate::{
    content::{
        downloader::{DownloadError, ZipDownloader},
        exclusion::get_exclusion_list,
        zip::{self, CompressionType, ZipError, ZipFileEntry},
        ContentService,
    },
    core::{
        auth::storage::LockedAuthStorage,
        manifest::{self, ManifestError, MANIFEST_RELATIVE_PATH},
        service_layer::ServiceLayerError,
        MaximaEvent,
    },
    gameinfo::GameInstallInfo,
    util::native::{maxima_dir, NativeError},
};

const QUEUE_FILE: &str = "download_queue.json";
const MAX_CONCURRENT_DOWNLOADS: usize = 16;

/// Filename of the completion marker written into a game's install
/// directory when ContentManager observes the download as `is_done()`.
///
/// External launchers (e.g. on macOS/CrossOver) poll for
/// this file's presence to decide that an install is **truly**
/// complete — not just that the game's exe exists. "Exe exists" can
/// be true mid-download for size-padded files or partially-extracted
/// zip entries; the marker is only written after the downloader
/// settled.
///
/// Schema is JSON with a `schema` integer for forward-compat. See
/// `InstallMarker` for the v1 fields.
pub const INSTALL_MARKER_FILENAME: &str = "FInstall.txt";

/// Contents of the install-completion marker file (`FInstall.txt`).
/// Written into the game's install directory by `ContentManager::update`
/// after a download transitions to `is_done()`.
///
/// Public so other crates in the workspace (and external consumers via
/// `maxima-lib` as a dependency) can deserialize the marker without
/// redefining the schema. Forward-compat: callers should accept any
/// `schema >= 1` and ignore unknown fields.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct InstallMarker {
    /// Forward-compat schema version. Consumers should accept >=1 and
    /// ignore unknown fields.
    pub schema: u32,
    /// The offer the install was queued against (e.g.
    /// `Origin.OFR.50.0000001`).
    pub offer_id: String,
    /// The build that landed on disk (lets consumers tell whether the
    /// installed copy matches the current live build later).
    pub build_id: String,
    /// Absolute path the install was written to. Self-describing —
    /// callers can verify the file is the one they expected.
    pub install_path: String,
    /// RFC 3339 UTC timestamp.
    pub completed_at: String,
    /// `maxima-lib` package version that wrote this marker. Cosmetic.
    pub maxima_lib_version: String,
}

#[derive(Debug, Default, Builder, Getters, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueuedGame {
    offer_id: String,
    build_id: String,
    path: PathBuf,
    /// Library slug: keys the install record and the per-game exclusion
    /// file. Empty for entries queued by older versions, which then get
    /// neither.
    #[builder(default)]
    #[serde(default)]
    slug: String,
    /// Wine prefix (unix) the game is installed into / will run in. Carried
    /// with the queue entry so the touchup and the install record use the
    /// prefix chosen when the install was requested, however long the
    /// download is queued.
    #[builder(default)]
    #[serde(default)]
    wine_prefix: Option<PathBuf>,
    /// Extra glob patterns (on top of the game's exclusion file) for files
    /// that must not be downloaded.
    #[builder(default)]
    #[serde(default)]
    exclude: Vec<String>,
    /// Locale to record in the install record (e.g. `en_US`).
    #[builder(default)]
    #[serde(default)]
    locale: Option<String>,
}

#[derive(Default, Getters, Serialize, Deserialize)]
pub struct DownloadQueue {
    current: Option<QueuedGame>,
    paused: bool,

    queued: Vec<QueuedGame>,
    completed: Vec<QueuedGame>,
}

#[derive(Error, Debug)]
pub enum ContentManagerError {
    #[error(transparent)]
    Downloader(#[from] DownloaderError),
    #[error(transparent)]
    Native(#[from] NativeError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error("download in progress, you must cancel it before starting a new one")]
    DownloadInProgress,
}

#[derive(Error, Debug)]
pub enum DownloaderError {
    #[error(transparent)]
    ServiceLayer(#[from] ServiceLayerError),
    #[error(transparent)]
    Zip(#[from] ZipError),
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Download(#[from] DownloadError),
    #[error(transparent)]
    Native(#[from] NativeError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),

    #[error("path `{0}` is not absolute")]
    PathNotAbsolute(PathBuf),
    #[error("failed to download range: {0}")]
    Http(StatusCode),
    #[error("requested length ({requested}) exceeds entry size ({entry})")]
    EntrySize { requested: u64, entry: usize },
    #[error("unsupported compression type `{0:?}`")]
    CompressionType(CompressionType),
    #[error("{failed} of {total} files failed to download")]
    FilesFailed { failed: usize, total: usize },
}

impl DownloadQueue {
    pub(crate) async fn load() -> Result<DownloadQueue, ContentManagerError> {
        let file = maxima_dir()?.join(QUEUE_FILE);
        if !file.exists() {
            return Ok(Self::default());
        }

        let data = fs::read_to_string(&file).await?;
        match serde_json::from_str(&data) {
            Ok(queue) => Ok(queue),
            Err(err) => {
                let backup = file.with_extension("json.bak");
                error!("Corrupt download queue, moved to {}: {err}", backup.display());
                let _ = fs::rename(&file, &backup).await;
                Ok(Self::default())
            }
        }
    }

    pub(crate) async fn save(&self) -> Result<(), ContentManagerError> {
        let file = maxima_dir()?.join(QUEUE_FILE);
        fs::write(file, serde_json::to_string(&self)?).await?;
        Ok(())
    }

    pub fn push_to_current(&mut self, game: QueuedGame) {
        if let Some(current) = self.current.take() {
            self.queued.insert(0, current);
        }
        self.current = Some(game);
    }

    fn pop_next(&mut self) -> Option<QueuedGame> {
        (!self.queued.is_empty()).then(|| self.queued.remove(0))
    }

    fn forget(&mut self, offer_id: &str) {
        if self.current.as_ref().is_some_and(|g| g.offer_id == offer_id) {
            self.current = None;
        }
        self.queued.retain(|g| g.offer_id != offer_id);
    }
}

pub struct GameDownloader {
    offer_id: String,
    install_info: GameInstallInfo,
    slug: String,
    wine_prefix: Option<PathBuf>,

    downloader: Arc<ZipDownloader>,
    entries: Vec<ZipFileEntry>,

    cancel_token: CancellationToken,
    completed_bytes: Arc<AtomicUsize>,
    total_bytes: usize,
    failure: Arc<Mutex<Option<String>>>,
    notify: Arc<Notify>,
}

impl GameDownloader {
    pub async fn new(
        content_service: &ContentService,
        game: &QueuedGame,
    ) -> Result<Self, DownloaderError> {
        let url = content_service
            .download_url(&game.offer_id, Some(&game.build_id))
            .await?;

        debug!("URL: {}", url.url());

        let downloader = ZipDownloader::new(&game.offer_id, &url.url(), &game.path).await?;

        let exclusion = get_exclusion_list(&game.slug, &game.exclude);
        let mut entries = Vec::new();
        let mut excluded = 0usize;
        for ele in downloader.manifest().entries() {
            if exclusion.is_match(ele.name()) {
                excluded += 1;
                continue;
            }
            entries.push(ele.clone());
        }
        if excluded > 0 {
            info!(
                "Excluding {} file(s) from the download ({} pattern(s))",
                excluded,
                exclusion.patterns().len()
            );
        }

        let mut install_info = GameInstallInfo::new(game.path.clone(), game.wine_prefix.clone())
            .with_offer(&game.offer_id, Some(&game.build_id))
            .with_exclude(game.exclude.clone());
        if let Some(locale) = &game.locale {
            install_info = install_info.with_locale(locale);
        }
        if !game.slug.is_empty() {
            install_info = install_info.with_slug(&game.slug);
        }

        let total_bytes = entries
            .iter()
            .map(|x| *x.compressed_size() as usize)
            .sum::<usize>()
            + 1; // the final unit is the touchup step

        Ok(GameDownloader {
            offer_id: game.offer_id.to_owned(),
            install_info,
            slug: game.slug.clone(),
            wine_prefix: game.wine_prefix.clone(),

            downloader: Arc::new(downloader),
            entries,
            cancel_token: CancellationToken::new(),
            completed_bytes: Arc::new(AtomicUsize::new(0)),
            total_bytes,
            failure: Arc::new(Mutex::new(None)),
            notify: Arc::new(Notify::new()),
        })
    }

    pub fn download(&self) {
        let downloader = self.downloader.clone();
        let entries = self.entries.clone();
        let cancel_token = self.cancel_token.clone();
        let completed_bytes = self.completed_bytes.clone();
        let failure = self.failure.clone();
        let notify = self.notify.clone();
        let offer_id = self.offer_id.clone();
        let install_info = self.install_info.clone();
        let slug = self.slug.clone();
        let wine_prefix = self.wine_prefix.clone();

        tokio::spawn(async move {
            let result = GameDownloader::start_downloads(
                downloader,
                entries,
                cancel_token,
                completed_bytes,
                install_info,
                slug,
                wine_prefix,
            )
            .await;
            if let Err(err) = result {
                error!("Install of {offer_id} failed: {err}");
                *failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(err.to_string());
            }
            notify.notify_one();
        });
    }

    async fn start_downloads(
        downloader: Arc<ZipDownloader>,
        entries: Vec<ZipFileEntry>,
        cancel_token: CancellationToken,
        completed_bytes: Arc<AtomicUsize>,
        mut install_info: GameInstallInfo,
        slug: String,
        wine_prefix: Option<PathBuf>,
    ) -> Result<(), DownloaderError> {
        let total = entries.len();
        let failed = Arc::new(AtomicUsize::new(0));

        let downloads = entries.into_iter().map(|ele| {
            let downloader = downloader.clone();
            let cancel_token = cancel_token.clone();
            let completed_bytes = completed_bytes.clone();
            let failed = failed.clone();

            async move {
                tokio::select! {
                    result = downloader.download_single_file(&ele, Some(Box::new(move |bytes| {
                        completed_bytes.fetch_add(bytes, Ordering::SeqCst);
                    }))) => {
                        if let Err(err) = result {
                            error!("Download of {} failed: {}", ele.name(), err);
                            failed.fetch_add(1, Ordering::SeqCst);
                        }
                    },
                    _ = cancel_token.cancelled() => {},
                }
            }
        });

        futures::stream::iter(downloads)
            .buffer_unordered(MAX_CONCURRENT_DOWNLOADS)
            .collect::<Vec<()>>()
            .await;

        if cancel_token.is_cancelled() {
            info!("Download cancelled; finished files are kept and skipped next time");
            return Ok(());
        }

        let failed = failed.load(Ordering::SeqCst);
        if failed > 0 {
            return Err(DownloaderError::FilesFailed { failed, total });
        }

        let path = downloader.path();
        info!("Files downloaded");

        // From here on the game is "installed": the record, not any
        // registry, is what says so (and which prefix it lives in).
        if !slug.is_empty() {
            install_info.save_to_json(&slug);
        }

        info!("Running touchup...");
        let manifest = manifest::read(path.join(MANIFEST_RELATIVE_PATH)).await?;

        if !slug.is_empty() {
            if let Some(version) = manifest.version() {
                install_info.version = Some(version);
                install_info.save_to_json(&slug);
            }
        }

        manifest.run_touchup(path, wine_prefix.as_deref()).await?;
        info!("Installation finished!");

        completed_bytes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    pub fn cancel(&self) {
        info!("Stopping installation of {}", self.offer_id);
        self.cancel_token.cancel();
    }

    /// Resolves once the download task has stopped — finished, failed or cancelled.
    pub async fn wait(&self) {
        self.notify.notified().await;
    }

    /// Why the install failed, once it has.
    pub fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn is_done(&self) -> bool {
        // `>=` (not `==`): the per-file `BytesDownloadedCallback` adds to
        // `completed_bytes` on every successful chunk read inside
        // `ByteCountingStream`. When a file's download is retried (see
        // `EntryDownloadRequest::download` — up to 6 attempts on a single
        // file under the v0.12.1 retry layer), each attempt streams bytes
        // through that callback before its eventual outcome — so the
        // counter ends up at `N × bytes_per_attempt` for an N-retry file
        // rather than exactly `compressed_size`. With `==` semantics, a
        // single retried file pushed the counter past `total_bytes` and
        // `is_done()` returned false forever — install hung silently
        // forever after "Installation finished!" landed in the log.
        // Found while debugging an install where `general_stream_patch_2.mstr`
        // hit 6 retries and over-counted by ~25MB.
        self.completed_bytes.load(Ordering::SeqCst) >= self.total_bytes
    }

    pub fn percentage_done(&self) -> f64 {
        let completed = self.completed_bytes.load(Ordering::SeqCst);
        (completed as f64 / self.total_bytes as f64) * 100.0
    }

    pub fn bytes_downloaded(&self) -> usize {
        self.completed_bytes.load(Ordering::SeqCst)
    }

    pub fn bytes_total(&self) -> usize {
        self.total_bytes
    }

    pub fn offer_id(&self) -> &String {
        &self.offer_id
    }

    async fn stop(self) {
        self.cancel();
        self.wait().await;
    }
}

#[derive(Getters)]
pub struct ContentManager {
    queue: DownloadQueue,
    service: ContentService,
    current: Option<GameDownloader>,
    #[getter(skip)]
    resume: bool,
}

impl ContentManager {
    pub async fn new(auth: LockedAuthStorage, resume: bool) -> Result<Self, ContentManagerError> {
        let mut queue = DownloadQueue::load().await?;
        if !resume {
            // An install interrupted by a previous exit waits at the front of
            // the queue instead of restarting behind the user's back.
            if let Some(stale) = queue.current.take() {
                queue.queued.insert(0, stale);
            }
        }
        Ok(Self {
            queue,
            service: ContentService::new(auth),
            current: None,
            resume,
        })
    }

    /// Start `game` now if nothing is downloading, otherwise queue it.
    pub async fn add_install(&mut self, game: QueuedGame) -> Result<(), ContentManagerError> {
        if self.current.is_none() {
            self.queue.forget(&game.offer_id);
            self.install_direct(game).await
        } else {
            self.queue.queued.retain(|g| g.offer_id != game.offer_id);
            self.queue.queued.push(game);
            self.queue.save().await
        }
    }

    /// Start `game` immediately. Whatever was downloading is stopped and goes
    /// back to the front of the queue.
    pub async fn install_now(&mut self, game: QueuedGame) -> Result<(), ContentManagerError> {
        if let Some(current) = self.current.take() {
            current.stop().await;
        }
        if let Some(previous) = self.queue.current.take() {
            if previous.offer_id != game.offer_id {
                self.queue.queued.insert(0, previous);
            }
        }
        self.queue.forget(&game.offer_id);
        self.queue.paused = false;
        self.install_direct(game).await
    }

    async fn install_direct(&mut self, game: QueuedGame) -> Result<(), ContentManagerError> {
        if self.current.is_some() {
            return Err(ContentManagerError::DownloadInProgress);
        }

        self.queue.current = Some(game.clone());
        self.queue.save().await?;

        let downloader = GameDownloader::new(&self.service, &game).await?;
        downloader.download();
        self.current = Some(downloader);
        Ok(())
    }

    /// Stop the download of `offer_id` if it's running and drop it from the queue.
    pub async fn cancel_install(&mut self, offer_id: &str) -> Result<(), ContentManagerError> {
        if self.current.as_ref().is_some_and(|c| c.offer_id == offer_id) {
            if let Some(current) = self.current.take() {
                current.stop().await;
            }
        }
        self.queue.forget(offer_id);
        self.queue.save().await
    }

    /// Stop the running download and hold the queue until `resume_queue`.
    /// The paused game stays first in line; files it already finished are skipped when it restarts.
    pub async fn pause_install(&mut self) -> Result<(), ContentManagerError> {
        if let Some(current) = self.current.take() {
            current.stop().await;
        }
        if let Some(paused) = self.queue.current.take() {
            self.queue.queued.insert(0, paused);
        }
        self.queue.paused = true;
        self.queue.save().await
    }

    /// Lift a pause and start the next queued install, if any.
    pub async fn resume_queue(&mut self) -> Result<(), ContentManagerError> {
        self.queue.paused = false;
        if self.current.is_none() {
            if let Some(next) = self.queue.pop_next() {
                return self.install_direct(next).await;
            }
        }
        self.queue.save().await
    }

    /// Start a queued install right away, stopping (and requeueing) the
    /// current one.
    pub async fn move_install_to_top(&mut self, offer_id: &str) -> Result<(), ContentManagerError> {
        if self.queue.current.as_ref().is_some_and(|g| g.offer_id == offer_id) {
            return Ok(());
        }
        let Some(index) = self.queue.queued.iter().position(|g| g.offer_id == offer_id) else {
            return Ok(());
        };
        let game = self.queue.queued.remove(index);
        self.install_now(game).await
    }

    pub(crate) async fn update(&mut self) -> Result<Option<MaximaEvent>, ContentManagerError> {
        let mut event = None;

        if let Some(current) = &self.current {
            let failure = current.failure();
            if failure.is_none() && !current.is_done() {
                return Ok(None);
            }

            let offer_id = current.offer_id.to_owned();
            let finished = self.queue.current.take();
            self.current = None;

            match failure {
                Some(message) => {
                    event = Some(MaximaEvent::InstallFailed { offer_id, message });
                }
                None => {
                    if let Some(game) = finished {
                        // Best-effort: the files are on disk either way.
                        if let Err(err) = write_install_marker(&game).await {
                            warn!(
                                "Failed to write {} for offer_id={} at {}: {}",
                                INSTALL_MARKER_FILENAME,
                                game.offer_id,
                                game.path.display(),
                                err
                            );
                        }
                        self.queue.completed.retain(|g| g.offer_id != game.offer_id);
                        self.queue.completed.push(game);
                    }
                    event = Some(MaximaEvent::InstallFinished(offer_id));
                }
            }

            self.queue.save().await?;
            self.advance().await?;
        } else if self.resume {
            self.resume = false;
            if let Some(game) = self.queue.current.take() {
                self.install_direct(game).await?;
            } else {
                self.advance().await?;
            }
        }

        Ok(event)
    }

    async fn advance(&mut self) -> Result<(), ContentManagerError> {
        if self.current.is_some() || self.queue.paused {
            return Ok(());
        }
        match self.queue.pop_next() {
            Some(next) => self.install_direct(next).await,
            None => Ok(()),
        }
    }
}

/// Write `<install_path>/FInstall.txt` describing a just-completed
/// install. Idempotent — overwrites any prior marker (re-installs
/// land here too, so the freshest install wins).
///
/// Returns the same `std::io::Error` family `tokio::fs` does;
/// callers should log + ignore (we don't want a failed marker write
/// to bubble up as an install-finished failure given the install
/// itself succeeded).
async fn write_install_marker(game: &QueuedGame) -> std::io::Result<()> {
    let marker = InstallMarker {
        schema: 1,
        offer_id: game.offer_id.clone(),
        build_id: game.build_id.clone(),
        install_path: game.path.to_string_lossy().into_owned(),
        completed_at: Utc::now().to_rfc3339(),
        maxima_lib_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let body = serde_json::to_string_pretty(&marker)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    // The install path itself should already exist (the downloader
    // wrote files into it). `create_dir_all` is defensive — covers
    // the edge case of an empty manifest where the dir wasn't touched.
    fs::create_dir_all(&game.path).await?;

    let marker_path = game.path.join(INSTALL_MARKER_FILENAME);
    fs::write(&marker_path, body).await?;
    info!("Wrote install marker: {}", marker_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(id: &str) -> QueuedGame {
        QueuedGame {
            offer_id: id.into(),
            build_id: "b".into(),
            path: PathBuf::from("/g"),
            ..Default::default()
        }
    }

    #[test]
    fn queue_is_first_in_first_out() {
        let mut queue = DownloadQueue::default();
        queue.queued = vec![game("a"), game("b"), game("c")];
        assert_eq!(queue.pop_next().unwrap().offer_id, "a");
        assert_eq!(queue.pop_next().unwrap().offer_id, "b");
    }

    #[test]
    fn interrupted_install_goes_back_to_the_front() {
        let mut queue = DownloadQueue::default();
        queue.queued = vec![game("b")];
        queue.push_to_current(game("a"));
        queue.push_to_current(game("c"));
        assert_eq!(queue.current.as_ref().unwrap().offer_id, "c");
        assert_eq!(queue.pop_next().unwrap().offer_id, "a");
    }

    #[test]
    fn forget_drops_current_and_queued_entries() {
        let mut queue = DownloadQueue::default();
        queue.current = Some(game("a"));
        queue.queued = vec![game("a"), game("b")];
        queue.forget("a");
        assert!(queue.current.is_none());
        assert_eq!(queue.queued.len(), 1);
    }

    #[test]
    fn queue_entries_saved_by_older_versions_still_load() {
        // download_queue.json from before slugs / prefixes / exclusion existed.
        let old = r#"{
            "current": {"offer_id": "Origin.OFR.1", "build_id": "7", "path": "/g/one"},
            "paused": false,
            "queued": [{"offer_id": "Origin.OFR.2", "build_id": "8", "path": "/g/two"}],
            "completed": []
        }"#;
        let queue: DownloadQueue = serde_json::from_str(old).unwrap();
        let current = queue.current.unwrap();
        assert_eq!(current.slug, "");
        assert_eq!(current.wine_prefix, None);
        assert!(current.exclude.is_empty());
        assert_eq!(current.locale, None);
        assert_eq!(queue.queued.len(), 1);
    }

    #[test]
    fn two_games_queue_with_their_own_prefixes() {
        let a = QueuedGameBuilder::default()
            .offer_id("Origin.OFR.1".to_owned())
            .build_id("1".to_owned())
            .path("/g/a".into())
            .slug("game-a".to_owned())
            .wine_prefix(Some("/prefixes/a".into()))
            .exclude(vec!["*.bik".to_owned()])
            .build()
            .unwrap();
        let b = QueuedGameBuilder::default()
            .offer_id("Origin.OFR.2".to_owned())
            .build_id("1".to_owned())
            .path("/g/b".into())
            .slug("game-b".to_owned())
            .wine_prefix(Some("/prefixes/b".into()))
            .build()
            .unwrap();
        assert_ne!(a, b);
        assert_eq!(a.wine_prefix(), &Some(PathBuf::from("/prefixes/a")));
        assert_eq!(b.wine_prefix(), &Some(PathBuf::from("/prefixes/b")));

        // They survive the on-disk queue round trip independently.
        let queue = DownloadQueue {
            current: Some(a.clone()),
            paused: false,
            queued: vec![b.clone()],
            completed: vec![],
        };
        let back: DownloadQueue =
            serde_json::from_str(&serde_json::to_string(&queue).unwrap()).unwrap();
        assert_eq!(back.current.as_ref(), Some(&a));
        assert_eq!(back.queued, vec![b]);
    }

    #[test]
    fn builder_defaults_keep_existing_call_sites_valid() {
        let game = QueuedGameBuilder::default()
            .offer_id("o".to_owned())
            .build_id("b".to_owned())
            .path("/p".into())
            .build()
            .unwrap();
        assert_eq!(game.slug(), "");
        assert_eq!(game.wine_prefix(), &None);
    }
}
