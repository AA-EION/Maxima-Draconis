//! App-side glue for the animated game background: remembers which games have
//! a background video URL, drives the [`Player`] and paints its frames behind
//! the UI with plain egui painting (no renderer-specific code).

use super::media_player::Player;
use crate::bridge_thread::MaximaLibRequest;
use egui::{pos2, Color32, Rect};
use log::warn;
use std::{collections::HashMap, sync::mpsc::Sender};

enum UrlState {
    Requested,
    Absent,
    Url(String),
}

#[derive(Default)]
pub struct BgVideo {
    player: Option<Player>,
    urls: HashMap<String, UrlState>,
}

impl BgVideo {
    pub fn set_url(&mut self, slug: String, url: Option<String>) {
        let state = match url {
            Some(url) => UrlState::Url(url),
            None => UrlState::Absent,
        };
        self.urls.insert(slug, state);
    }

    /// Paint the background video for `slug` into `rect` and return `true`, or
    /// return `false` (painting nothing) so the caller keeps its static
    /// background. `playing` is whether the video is wanted at all right now
    /// (e.g. the Games page is showing); `fade` scales its opacity.
    pub fn draw(
        &mut self,
        ui: &egui::Ui,
        rect: Rect,
        slug: &str,
        enabled: bool,
        playing: bool,
        fade: f32,
        backend: &Sender<MaximaLibRequest>,
    ) -> bool {
        if !enabled {
            // dropping the player stops its decode thread
            self.player = None;
            return false;
        }

        let url = match self.urls.get(slug) {
            Some(UrlState::Url(url)) => url.as_str(),
            Some(UrlState::Absent) => {
                if let Some(player) = &mut self.player {
                    player.stop();
                }
                return false;
            }
            Some(UrlState::Requested) => return false,
            None => {
                if backend.send(MaximaLibRequest::GetGameBgVideoRequest(slug.to_owned())).is_err()
                {
                    warn!("bridge thread gone, cannot request a background video for {slug}");
                }
                self.urls.insert(slug.to_owned(), UrlState::Requested);
                return false;
            }
        };

        let ctx = ui.ctx();
        let player = self.player.get_or_insert_with(|| Player::new(ctx));
        player.start(url);

        let (focused, minimized) =
            ctx.input(|i| (i.viewport().focused, i.viewport().minimized));
        let hidden = focused == Some(false) || minimized == Some(true);
        player.set_paused(!playing || hidden);

        let Some((texture, size)) = player.poll() else {
            return false;
        };
        if fade <= 0.0 || size.x <= 0.0 || size.y <= 0.0 || rect.height() <= 0.0 {
            return false;
        }

        // "cover" fit: fill the rect, cropping whichever axis overflows
        let video_aspect = size.x / size.y;
        let rect_aspect = rect.width() / rect.height();
        let uv = if video_aspect > rect_aspect {
            let w = rect_aspect / video_aspect;
            Rect::from_min_max(pos2((1.0 - w) / 2.0, 0.0), pos2((1.0 + w) / 2.0, 1.0))
        } else {
            let h = video_aspect / rect_aspect;
            Rect::from_min_max(pos2(0.0, (1.0 - h) / 2.0), pos2(1.0, (1.0 + h) / 2.0))
        };
        // dimmed so foreground text stays readable over the moving image
        let tint = Color32::WHITE.gamma_multiply(0.7 * fade.min(1.0));
        ui.painter().image(texture, rect, uv, tint);
        true
    }
}
