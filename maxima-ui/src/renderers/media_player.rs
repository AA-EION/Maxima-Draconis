//! FFmpeg-backed video decoder that feeds a plain egui texture.
//!
//! Frames are decoded on a worker thread, scaled to RGBA and parked in a
//! single "latest frame" slot. The UI thread uploads whatever is in the slot
//! with `TextureHandle::set` when it polls, so nothing here touches the
//! graphics backend (works the same on wgpu and glow) and a UI that repaints
//! slower than the video simply drops frames instead of queueing uploads.

use egui::{ColorImage, TextureHandle, TextureId, TextureOptions, Vec2};
use ffmpeg::{
    codec,
    format::{self, Pixel},
    media::Type,
    software::scaling::{Context as Scaler, Flags},
    util::frame::Video,
};
use ffmpeg_next as ffmpeg;
use log::{debug, warn};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

/// Upper bound on published frames per second. Background videos are
/// decorative; anything past a typical display rate is wasted work.
const MAX_FPS: f64 = 60.0;
/// Frames are downscaled to at most this width before upload (the video sits
/// behind dimming, and 1080p RGBA uploads every frame are needlessly heavy).
const MAX_WIDTH: u32 = 1280;
/// How long a network read may stall before ffmpeg gives up (microseconds).
const NETWORK_TIMEOUT_US: &str = "15000000";
/// How often a paused decode thread re-checks its flags.
const PAUSE_POLL: Duration = Duration::from_millis(100);

fn ffmpeg_ready() -> bool {
    static READY: OnceLock<bool> = OnceLock::new();
    *READY.get_or_init(|| {
        if let Err(err) = ffmpeg::init() {
            warn!("Unable to initialize ffmpeg, background videos disabled: {err:?}");
            return false;
        }
        ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Error);
        format::network::init();
        true
    })
}

struct Session {
    stop: AtomicBool,
    paused: AtomicBool,
    failed: AtomicBool,
    frame: Mutex<Option<ColorImage>>,
}

pub struct Player {
    ctx: egui::Context,
    texture: TextureHandle,
    location: Option<String>,
    session: Option<Arc<Session>>,
    paused: bool,
    has_frame: bool,
    size: Vec2,
}

impl Player {
    pub fn new(ctx: &egui::Context) -> Self {
        let texture = ctx.load_texture(
            "bg-video",
            ColorImage::new([1, 1], egui::Color32::BLACK),
            TextureOptions::LINEAR,
        );
        Self {
            ctx: ctx.clone(),
            texture,
            location: None,
            session: None,
            paused: false,
            has_frame: false,
            size: Vec2::ZERO,
        }
    }

    /// Begin playing `location` (URL or file path) on a loop. Does nothing if it
    /// is already the current source, including when that source failed to
    /// open, so a bad URL is tried once rather than every frame.
    pub fn start(&mut self, location: &str) {
        if self.location.as_deref() == Some(location) {
            return;
        }
        self.stop();
        self.location = Some(location.to_owned());
        if !ffmpeg_ready() {
            return;
        }

        let session = Arc::new(Session {
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(self.paused),
            failed: AtomicBool::new(false),
            frame: Mutex::new(None),
        });
        self.session = Some(session.clone());

        let ctx = self.ctx.clone();
        let location = location.to_owned();
        let spawned = thread::Builder::new().name("bg-video".into()).spawn(move || {
            match decode_loop(&location, &session, &ctx) {
                Ok(()) => debug!("background video stopped: {location}"),
                Err(err) => {
                    warn!("background video unavailable ({location}): {err}");
                    session.failed.store(true, Ordering::Release);
                    ctx.request_repaint();
                }
            }
        });
        if let Err(err) = spawned {
            warn!("could not spawn background video thread: {err}");
            self.session = None;
        }
    }

    /// Stop playback and forget the source. The worker is signalled, not
    /// joined, so a stalled network read never blocks the UI.
    pub fn stop(&mut self) {
        if let Some(session) = self.session.take() {
            session.stop.store(true, Ordering::Release);
        }
        self.location = None;
        self.has_frame = false;
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        if let Some(session) = &self.session {
            session.paused.store(paused, Ordering::Release);
        }
    }

    /// Upload the newest decoded frame, if any, and return the texture to
    /// paint. `None` means there is nothing to show (not started, still
    /// opening, or failed) and the caller should use its static background.
    pub fn poll(&mut self) -> Option<(TextureId, Vec2)> {
        let session = self.session.as_ref()?;
        if session.failed.load(Ordering::Acquire) {
            self.has_frame = false;
            return None;
        }
        let frame = session.frame.lock().ok().and_then(|mut slot| slot.take());
        if let Some(image) = frame {
            self.size = Vec2::new(image.size[0] as f32, image.size[1] as f32);
            self.texture.set(image, TextureOptions::LINEAR);
            self.has_frame = true;
        }
        self.has_frame.then(|| (self.texture.id(), self.size))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Source {
    input: format::context::Input,
    decoder: ffmpeg::decoder::Video,
    index: usize,
    frame_interval: Duration,
}

fn open(location: &str) -> Result<Source, ffmpeg::Error> {
    let mut options = ffmpeg::Dictionary::new();
    if location.starts_with("http://") || location.starts_with("https://") {
        // A remote container must not be able to pull in local files or other protocols.
        options.set("protocol_whitelist", "http,https,tcp,tls,crypto");
        options.set("rw_timeout", NETWORK_TIMEOUT_US);
    }
    let input = format::input_with_dictionary(&location, options)?;
    let stream = input.streams().best(Type::Video).ok_or(ffmpeg::Error::StreamNotFound)?;
    let index = stream.index();
    let rate = stream.avg_frame_rate();
    let mut fps = rate.numerator() as f64 / rate.denominator().max(1) as f64;
    if !(1.0..=240.0).contains(&fps) {
        fps = 30.0;
    }
    let decoder =
        codec::context::Context::from_parameters(stream.parameters())?.decoder().video()?;
    Ok(Source { input, decoder, index, frame_interval: Duration::from_secs_f64(1.0 / fps) })
}

enum Flow {
    Continue,
    Stop,
}

struct Pacer<'a> {
    session: &'a Session,
    ctx: &'a egui::Context,
    scaler: Option<Scaler>,
    rgba: Video,
    deadline: Instant,
    last_publish: Option<Instant>,
}

impl Pacer<'_> {
    fn wait_while_paused(&mut self) -> Flow {
        while self.session.paused.load(Ordering::Acquire) {
            if self.session.stop.load(Ordering::Acquire) {
                return Flow::Stop;
            }
            thread::sleep(PAUSE_POLL);
            self.deadline = Instant::now();
        }
        if self.session.stop.load(Ordering::Acquire) {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }

    fn handle_frame(&mut self, frame: &Video, interval: Duration) -> Result<Flow, ffmpeg::Error> {
        if matches!(self.wait_while_paused(), Flow::Stop) {
            return Ok(Flow::Stop);
        }

        let now = Instant::now();
        if now < self.deadline {
            thread::sleep(self.deadline - now);
        }
        // never try to catch up after a stall: just resume at the normal rate
        self.deadline = (self.deadline + interval).max(Instant::now());

        let min_gap = Duration::from_secs_f64(1.0 / MAX_FPS);
        if self.last_publish.is_some_and(|t| t.elapsed() < min_gap) {
            return Ok(Flow::Continue);
        }

        let (w, h) = (frame.width(), frame.height());
        if w == 0 || h == 0 {
            return Ok(Flow::Continue);
        }
        let out_w = (w.min(MAX_WIDTH) & !1).max(2);
        let out_h = (((h as u64 * out_w as u64) / w as u64) as u32 & !1).max(2);

        match &mut self.scaler {
            Some(scaler) => {
                scaler.cached(frame.format(), w, h, Pixel::RGBA, out_w, out_h, Flags::BILINEAR)
            }
            none => {
                *none = Some(Scaler::get(
                    frame.format(),
                    w,
                    h,
                    Pixel::RGBA,
                    out_w,
                    out_h,
                    Flags::BILINEAR,
                )?)
            }
        }
        let scaler = self.scaler.as_mut().expect("scaler initialised above");
        scaler.run(frame, &mut self.rgba)?;

        let image = rgba_to_image(&self.rgba);
        if let Ok(mut slot) = self.session.frame.lock() {
            *slot = Some(image);
        }
        self.last_publish = Some(Instant::now());
        self.ctx.request_repaint();
        Ok(Flow::Continue)
    }

    /// Pull every frame the decoder has ready. Returns how many were handled,
    /// or `None` if playback was asked to stop.
    fn drain(
        &mut self,
        decoder: &mut ffmpeg::decoder::Video,
        interval: Duration,
    ) -> Result<Option<u32>, ffmpeg::Error> {
        let mut decoded = Video::empty();
        let mut count = 0;
        while decoder.receive_frame(&mut decoded).is_ok() {
            if matches!(self.handle_frame(&decoded, interval)?, Flow::Stop) {
                return Ok(None);
            }
            count += 1;
        }
        Ok(Some(count))
    }
}

fn decode_loop(
    location: &str,
    session: &Session,
    ctx: &egui::Context,
) -> Result<(), ffmpeg::Error> {
    let mut source = open(location)?;
    let mut pacer = Pacer {
        session,
        ctx,
        scaler: None,
        rgba: Video::empty(),
        deadline: Instant::now(),
        last_publish: None,
    };

    loop {
        let mut frames_in_pass = 0u32;
        let interval = source.frame_interval;
        let Source { input, decoder, index, .. } = &mut source;

        for (stream, packet) in input.packets() {
            if session.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            if stream.index() != *index {
                continue;
            }
            if decoder.send_packet(&packet).is_err() {
                continue;
            }
            match pacer.drain(decoder, interval)? {
                Some(n) => frames_in_pass += n,
                None => return Ok(()),
            }
        }

        let _ = decoder.send_eof();
        match pacer.drain(decoder, interval)? {
            Some(n) => frames_in_pass += n,
            None => return Ok(()),
        }

        if frames_in_pass == 0 {
            // an undecodable stream would otherwise spin forever
            return Err(ffmpeg::Error::InvalidData);
        }

        // loop: rewind in place when the source allows it, reopen otherwise
        if source.input.seek(0, ..).is_ok() {
            source.decoder.flush();
        } else {
            source = open(location)?;
        }
    }
}

fn rgba_to_image(frame: &Video) -> ColorImage {
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let stride = frame.stride(0);
    let data = frame.data(0);
    let row = w * 4;
    let mut bytes = Vec::with_capacity(row * h);
    for line in 0..h {
        let start = line * stride;
        bytes.extend_from_slice(&data[start..start + row]);
    }
    ColorImage::from_rgba_unmultiplied([w, h], &bytes)
}
