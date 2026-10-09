use super::{
    auth::storage::LockedAuthStorage,
    locale::Locale,
    manifest::{self, GameManifest, ManifestError, MANIFEST_RELATIVE_PATH},
    service_layer::{
        ServiceGameProductType, ServiceGetLegacyCatalogDefsRequestBuilder,
        ServiceGetPreloadedOwnedGamesRequest, ServiceGetPreloadedOwnedGamesRequestBuilder,
        ServiceGetPreloadedOwnedGamesRequestBuilderError, ServiceLayerClient, ServiceLayerError,
        ServiceLegacyOffer, ServicePlatform, ServiceStorefront, ServiceUser,
        ServiceUserGameProduct, SERVICE_REQUEST_GETLEGACYCATALOGDEFS,
        SERVICE_REQUEST_GETPRELOADEDOWNEDGAMES,
    },
};
#[cfg(unix)]
use crate::unix::fs::case_insensitive_path;
use crate::gameinfo::{self, GameInstallInfo};
use crate::util::native::{NativeError, SafeStr};
#[cfg(not(unix))]
use crate::util::registry::{parse_partial_registry_path, parse_registry_path};
use crate::util::registry::RegistryError;
use derive_getters::Getters;
use log::{debug, warn};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::SystemTimeError,
};
use thiserror::Error;

/// How far above a registry-resolved path to look for `__Installer`; the
/// path may name the game's exe (`<dir>/bin/x64/game.exe`) rather than its
/// install dir.
const MANIFEST_SEARCH_DEPTH: usize = 5;

fn is_unresolved_registry_path(path: &Path) -> bool {
    path.to_string_lossy().starts_with('[')
}

/// Resolve a manifest path against a known install root, with no registry
/// involved: `[HKLM\...\Install Dir]bin\game.exe` becomes
/// `<root>/bin/game.exe` (everything up to the last `]` is the registry
/// placeholder the install root replaces); a plain relative path is joined
/// as is.
pub fn path_in_install_root(root: &Path, key: &str) -> PathBuf {
    let relative = match key.rfind(']') {
        Some(end) => &key[end + 1..],
        None => key,
    };
    let relative = relative.trim_start_matches(['/', '\\']);
    let relative = if cfg!(unix) {
        relative.replace('\\', "/")
    } else {
        relative.to_owned()
    };

    let joined = root.join(relative);
    #[cfg(unix)]
    let joined = case_insensitive_path(joined);
    joined
}

fn has_registry_placeholder(key: &str) -> bool {
    key.contains('[') && key.contains(']')
}

fn find_manifest_near(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .take(MANIFEST_SEARCH_DEPTH)
        .filter(|dir| !dir.as_os_str().is_empty())
        .find_map(|dir| {
            let candidate = dir.join(MANIFEST_RELATIVE_PATH);
            #[cfg(unix)]
            let candidate = case_insensitive_path(candidate);
            candidate.is_file().then_some(candidate)
        })
}

#[derive(Error, Debug)]
pub enum LibraryError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    ServiceGetPreloadedOwnedGamesRequestBuilderError(
        #[from] ServiceGetPreloadedOwnedGamesRequestBuilderError,
    ),
    #[error(transparent)]
    Native(#[from] NativeError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    ServiceLayer(#[from] ServiceLayerError),
    #[error(transparent)]
    Time(#[from] SystemTimeError),

    #[error("`{0}` has no manifest found")]
    NoManifest(String),
    #[error("`{0}` was not installed")]
    NotInstalled(String),
    #[error("`{0}`'s execute path was not found")]
    NoPath(String),
    #[error("`{0}`'s version info is unavailable")]
    NoVersion(String),
}

#[derive(Clone, Getters)]
pub struct OwnedOffer {
    slug: String,
    product: ServiceUserGameProduct,
    offer: ServiceLegacyOffer,
}

impl OwnedOffer {
    /// The install record for this game (`gameinfo/<slug>.json`), if one was
    /// written by an install or a locate. Independent of any Wine prefix.
    pub fn install_info(&self) -> Option<GameInstallInfo> {
        gameinfo::load_game_info(&self.slug)
    }

    /// The recorded install directory, when it still exists on disk.
    pub fn install_dir(&self) -> Option<PathBuf> {
        self.install_info()
            .map(|info| info.path)
            .filter(|path| path.is_dir())
    }

    /// The Wine prefix this game runs in (unix), without creating it.
    #[cfg(unix)]
    pub fn wine_prefix(&self) -> Option<PathBuf> {
        crate::unix::prefix::prefix_for_game(&self.slug, None).ok()
    }

    /// Resolve a `[REGISTRY]relative` key for this game: against its
    /// recorded install dir when there is one, otherwise through the
    /// registry of THIS game's prefix (unix) or the machine registry
    /// (Windows). `partial` asks for just the install directory.
    async fn resolve_key(&self, key: &str, partial: bool) -> Result<PathBuf, RegistryError> {
        if has_registry_placeholder(key) {
            if let Some(dir) = self.install_dir() {
                return Ok(if partial {
                    dir
                } else {
                    path_in_install_root(&dir, key)
                });
            }
        }

        #[cfg(unix)]
        {
            let prefix = crate::unix::prefix::prefix_for_game(&self.slug, None)?;
            if partial {
                crate::unix::wine::parse_partial_registry_path_in(&prefix, key).await
            } else {
                crate::unix::wine::parse_registry_path_in(&prefix, key).await
            }
        }
        #[cfg(not(unix))]
        {
            if partial {
                parse_partial_registry_path(key).await
            } else {
                parse_registry_path(key).await
            }
        }
    }

    pub async fn is_installed(&self) -> bool {
        // A record of a successful install (or locate) settles it without
        // touching any registry.
        if self.install_dir().is_some() {
            return true;
        }

        // I would love to throw an error here but that's just not feasible.
        // If you can't grab the path it may as well not be installed.
        let Some(path) = &self.offer.install_check_override().as_ref() else {
            return false;
        };

        if let Ok(path) = self.resolve_key(path, false).await {
            // If it wasn't replaced...
            if !is_unresolved_registry_path(&path) {
                #[cfg(unix)]
                let path = case_insensitive_path(path);
                if path.exists() {
                    return true;
                }
            }
        }

        // The exact file the override names can be missing (renamed exe, a
        // different layout) while the game is still there; the installer
        // manifest near the registered install dir is proof enough.
        matches!(self.manifest_path().await, Ok(Some(_)))
    }

    pub async fn install_check_path(&self) -> Result<String, ManifestError> {
        let check = self
            .offer
            .install_check_override()
            .as_ref()
            .ok_or(ManifestError::NoInstallPath(self.slug.clone()))?;
        Ok(self.resolve_key(check, false).await?.safe_str()?.to_owned())
    }

    pub async fn execute_path(&self, trial: bool) -> Result<PathBuf, LibraryError> {
        let manifest = match self.local_manifest().await? {
            Some(manifest) => manifest,
            None => return Err(LibraryError::NoManifest(self.slug.clone())),
        };

        let path = if let Some(path) = manifest.execute_path(trial) {
            &Some(path)
        } else {
            self.offer.execute_path_override()
        };

        if let Some(path) = path {
            Ok(self.resolve_key(path, false).await?)
        } else {
            Err(LibraryError::NoPath(self.slug.clone()))
        }
    }

    pub async fn installed_version(&self) -> Result<String, LibraryError> {
        // Deliberately not gated on `is_installed()`: that depends on the
        // registry-named file, while the version only needs the manifest.
        let manifest = match self.local_manifest().await {
            Ok(Some(manifest)) => manifest,
            Ok(None) => {
                return Err(if self.is_installed().await {
                    LibraryError::NoManifest(self.slug.clone())
                } else {
                    LibraryError::NotInstalled(self.slug.clone())
                })
            }
            Err(ManifestError::NoInstallPath(_)) => {
                return Err(LibraryError::NotInstalled(self.slug.clone()))
            }
            Err(err) => return Err(err.into()),
        };

        if let Some(version) = manifest.version() {
            Ok(version)
        } else {
            Err(LibraryError::NoVersion(self.slug.clone()))
        }
    }

    /// `Ok(None)` means no installer manifest could be located for this
    /// offer; a manifest that exists but cannot be parsed is an error.
    pub async fn local_manifest(&self) -> Result<Option<Box<dyn GameManifest>>, ManifestError> {
        let Some(path) = self.manifest_path().await? else {
            debug!("no installer manifest found for `{}`", self.slug);
            return Ok(None);
        };

        Ok(Some(manifest::read(path).await?))
    }

    async fn manifest_path(&self) -> Result<Option<PathBuf>, ManifestError> {
        if let Some(dir) = self.install_dir() {
            let candidate = dir.join(MANIFEST_RELATIVE_PATH);
            #[cfg(unix)]
            let candidate = case_insensitive_path(candidate);
            if candidate.is_file() {
                return Ok(Some(candidate));
            }
        }

        let check = self
            .offer
            .install_check_override()
            .as_ref()
            .ok_or(ManifestError::NoInstallPath(self.slug.clone()))?;

        let names_manifest = check.contains("installerdata.xml");
        let resolved = self.resolve_key(check, !names_manifest).await;
        let resolved = match resolved {
            Ok(path) if !is_unresolved_registry_path(&path) => path,
            Ok(_) => return Ok(None),
            Err(err) => {
                warn!("could not resolve install path of `{}`: {err}", self.slug);
                return Ok(None);
            }
        };

        if names_manifest {
            #[cfg(unix)]
            let resolved = case_insensitive_path(resolved);
            return Ok(resolved.is_file().then_some(resolved));
        }

        Ok(find_manifest_near(&resolved))
    }

    pub fn offer_id(&self) -> &String {
        self.offer.offer_id()
    }
}

#[derive(Clone, Getters)]
pub struct OwnedTitle {
    base_offer: OwnedOffer,
    offers: Vec<OwnedOffer>,
}

fn group_offers(products: Vec<OwnedOffer>) -> Vec<OwnedTitle> {
    let mut base_products = HashMap::new();
    let mut product_map = HashMap::new();

    for product in products {
        let slug = product
            .product
            .product()
            .base_item()
            .base_game_slug()
            .clone();

        let full_game = (|| {
            // Ensure it's the full game
            if product.offer.display_type() != "FullGame"
                || product
                    .product()
                    .product()
                    .base_item()
                    .game_type()
                    .as_ref()
                    .unwrap_or(&ServiceGameProductType::ExpansionPack)
                    != &ServiceGameProductType::BaseGame
            {
                return false;
            }

            if !product.offer.is_downloadable() {
                return false;
            }

            // Ensure it isn't a trial
            if product
                .product()
                .product()
                .game_product_user()
                .game_product_user_trial()
                .is_some()
            {
                return false;
            }

            true
        })();

        if full_game {
            match slug {
                Some(slug) => {
                    product_map
                        .entry(slug.clone())
                        .or_insert_with(Vec::new)
                        .push(product);
                }
                None => {
                    base_products.insert(product.slug.clone(), product.clone());
                }
            }
        }
    }

    let mut titles = Vec::new();

    for (base_slug, base_offer) in base_products {
        let associated_products = product_map.remove(&base_slug).unwrap_or_default();
        titles.push(OwnedTitle {
            base_offer,
            offers: associated_products,
        });
    }

    titles
}

impl OwnedTitle {
    pub fn new(base_offer: OwnedOffer, offers: Vec<OwnedOffer>) -> Self {
        Self { base_offer, offers }
    }

    pub fn name(&self) -> String {
        match self
            .base_offer
            .product
            .product()
            .base_item()
            .title()
            .as_ref()
        {
            Some(title) => title.replace("\n", "").to_owned(),
            None => "Unknown".to_owned(),
        }
    }

    pub fn base_game(&self) -> Option<OwnedOffer> {
        for offer in self.offers.iter() {
            if offer
                .product
                .product()
                .base_item()
                .game_type()
                .as_ref()
                .unwrap_or(&ServiceGameProductType::ExpansionPack)
                != &ServiceGameProductType::BaseGame
            {
                return None;
            }

            if !*offer.product.product().downloadable() {
                return None;
            }

            return Some(offer.clone());
        }

        None
    }

    pub fn offer(&self, slug: &str) -> Option<&OwnedOffer> {
        self.offers.iter().find(|x| x.slug == slug)
    }

    pub fn extra_offers(&self) -> &Vec<OwnedOffer> {
        &self.offers
    }
}

pub struct GameLibrary {
    service_layer: ServiceLayerClient,
    library: Vec<OwnedTitle>,
    last_request: u64,
}

impl GameLibrary {
    pub async fn new(auth: LockedAuthStorage) -> Self {
        Self {
            service_layer: ServiceLayerClient::new(auth),
            library: Vec::new(),
            last_request: 0,
        }
    }

    pub async fn games(&mut self) -> Result<&Vec<OwnedTitle>, LibraryError> {
        self.update_if_needed().await?;
        Ok(&self.library)
    }

    pub async fn title_by_base_offer(
        &mut self,
        offer_id: &str,
    ) -> Result<Option<&OwnedTitle>, LibraryError> {
        self.update_if_needed().await?;
        Ok(self
            .library
            .iter()
            .find(|x| x.base_offer.offer.offer_id() == offer_id))
    }

    pub async fn game_by_base_offer(
        &mut self,
        offer_id: &str,
    ) -> Result<Option<&OwnedOffer>, LibraryError> {
        self.update_if_needed().await?;
        Ok(self
            .library
            .iter()
            .find(|x| x.base_offer.offer.offer_id() == offer_id)
            .map(|x| &x.base_offer))
    }

    pub async fn game_by_base_slug(
        &mut self,
        slug: &str,
    ) -> Result<Option<&OwnedOffer>, LibraryError> {
        self.update_if_needed().await?;
        Ok(self
            .library
            .iter()
            .find(|x| x.base_offer.product.product().game_slug() == slug)
            .map(|x| &x.base_offer))
    }

    /// Resolve whatever a caller has (slug, offer id, Steam App ID, content
    /// id…) to the library's canonical base slug where possible. Falls back
    /// to the input when the library doesn't know it (not logged in,
    /// unlinked accounts). Used to key per-game state (e.g. CrossOver
    /// bottle names) consistently across input forms.
    pub async fn canonical_slug(&mut self, typed: &str) -> String {
        if let Ok(Some(offer)) = self.game_by_base_slug(typed).await {
            return offer.slug().clone();
        }
        if let Ok(Some(offer)) = self.game_by_base_offer(typed).await {
            return offer.slug().clone();
        }
        // Exhaustive scan — same property set maxima-cli's launch
        // resolution matches against.
        let typed_s = typed.to_string();
        if let Ok(games) = self.games().await {
            for game in games {
                let base = game.base_offer();
                if base.slug() == &typed_s
                    || base.offer_id() == &typed_s
                    || base.product().id() == &typed_s
                    || base.product().origin_offer_id() == &typed_s
                    || base.offer().content_id() == &typed_s
                    || base.product().product().id() == &typed_s
                {
                    return base.slug().clone();
                }
            }
        }
        typed_s
    }

    async fn update_if_needed(&mut self) -> Result<(), LibraryError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();

        if now - self.last_request > 1200 {
            self.request_owned_games().await?;
        }

        Ok(())
    }

    async fn request_owned_games(&mut self) -> Result<(), LibraryError> {
        self.request_page_concurrent(Locale::EnUs, 1).await?;

        Ok(())
    }

    async fn request_page_concurrent(
        &mut self,
        locale: Locale,
        page: u32,
    ) -> Result<(), LibraryError> {
        let responses: Vec<ServiceUserGameProduct> = {
            let request = GameLibrary::library_request(
                &locale,
                ServiceGameProductType::DigitalFullGame,
                true,
                page,
            )?;

            let user: ServiceUser = self
                .service_layer
                .request(SERVICE_REQUEST_GETPRELOADEDOWNEDGAMES, request)
                .await?;
            user.owned_game_products()
                .as_ref()
                .ok_or(ServiceLayerError::MissingField)?
                .items()
                .clone()
        };

        let offer_ids = responses
            .iter()
            .map(|x| x.origin_offer_id().to_owned())
            .collect();

        let defs: Vec<ServiceLegacyOffer> = self
            .service_layer
            .request(
                SERVICE_REQUEST_GETLEGACYCATALOGDEFS,
                ServiceGetLegacyCatalogDefsRequestBuilder::default()
                    .offer_ids(offer_ids)
                    .locale(locale)
                    .build()
                    .unwrap(),
            )
            .await?;

        let mut offers: Vec<OwnedOffer> = Vec::new();
        for product in responses {
            let def = match defs
                .iter()
                .find(|x| x.offer_id() == product.origin_offer_id())
            {
                None => {
                    continue;
                }
                Some(def) => def.clone(),
            };
            offers.push(OwnedOffer {
                slug: product.product().game_slug().to_owned(),
                product: product.clone(),
                offer: def,
            });
        }

        let mut titles = group_offers(offers);
        titles.sort_by(|a, b| a.name().to_lowercase().cmp(&b.name().to_lowercase()));

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();

        self.library = titles;
        self.last_request = now;
        Ok(())
    }

    fn library_request(
        locale: &Locale,
        r#type: ServiceGameProductType,
        entitlement_enabled: bool,
        page: u32,
    ) -> Result<ServiceGetPreloadedOwnedGamesRequest, LibraryError> {
        Ok(ServiceGetPreloadedOwnedGamesRequestBuilder::default()
            .is_mac(false)
            .locale(locale.to_owned())
            .limit(1000)
            .next(((page - 1) * 1000).to_string())
            .r#type(r#type)
            .entitlement_enabled(None)
            .storefronts(vec![
                ServiceStorefront::Ea,
                ServiceStorefront::Steam,
                ServiceStorefront::Epic,
            ])
            .platforms(vec![ServicePlatform::Pc])
            .build()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_is_found_from_an_exe_path() {
        let dir = std::env::temp_dir().join(format!("maxima-library-test-{}", std::process::id()));
        let installer = dir.join("__Installer");
        std::fs::create_dir_all(&installer).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(installer.join("installerdata.xml"), "<x/>").unwrap();

        let from_dir = find_manifest_near(&dir);
        let from_exe = find_manifest_near(&dir.join("bin").join("game.exe"));
        let elsewhere = find_manifest_near(Path::new("nonexistent-maxima-dir/game.exe"));
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(from_dir.is_some());
        assert_eq!(from_dir, from_exe);
        assert!(elsewhere.is_none());
    }

    #[test]
    fn manifest_paths_resolve_against_the_install_root() {
        let root = Path::new("/nonexistent-maxima/games/g");
        assert_eq!(
            path_in_install_root(root, r"[HKEY_LOCAL_MACHINE\SOFTWARE\EA Games\G\Install Dir]bin\game.exe"),
            PathBuf::from("/nonexistent-maxima/games/g/bin/game.exe")
        );
        assert_eq!(
            path_in_install_root(root, r"[HKEY_LOCAL_MACHINE\SOFTWARE\X\Install Dir]\game.exe"),
            PathBuf::from("/nonexistent-maxima/games/g/game.exe")
        );
        assert_eq!(
            path_in_install_root(root, "Game/launcher.exe"),
            PathBuf::from("/nonexistent-maxima/games/g/Game/launcher.exe")
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_root_resolution_matches_existing_case() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Bin")).unwrap();
        std::fs::write(dir.path().join("Bin").join("Game.EXE"), b"").unwrap();
        assert_eq!(
            path_in_install_root(dir.path(), r"[HKLM\X\Install Dir]bin\game.exe"),
            dir.path().join("Bin").join("Game.EXE")
        );
    }

    #[test]
    fn only_bracketed_keys_count_as_registry_placeholders() {
        assert!(has_registry_placeholder(r"[HKLM\X\Dir]a.exe"));
        assert!(!has_registry_placeholder(r"C:\Games\a.exe"));
    }

    #[test]
    fn unresolved_registry_paths_are_detected() {
        assert!(is_unresolved_registry_path(Path::new(
            r"[HKEY_LOCAL_MACHINE\SOFTWARE\X\Install Dir]\game.exe"
        )));
        assert!(!is_unresolved_registry_path(Path::new("/games/x")));
    }
}
