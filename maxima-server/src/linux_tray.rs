//! Linux StatusNotifierItem tray (feature `linux-tray`). Pure D-Bus via `ksni`
//! — no GTK. Menu: **Open Maxima** (`maxima` egui UI on `PATH`) / **Stop
//! Server**. On a host with no D-Bus / display, SNI registration fails and the
//! server keeps running headless. See docs/MACOS_BUNDLING.md.
//!
//! NOTE: built only under `--features linux-tray` on Linux; not built by the
//! current Linux CI job. `cargo check`ed for the target, but verify on a real
//! Linux desktop before relying on the runtime behaviour.

use ksni::menu::StandardItem;
use ksni::{MenuItem, Tray, TrayService};

use crate::status_icon::StopServer;

struct MaximaTray {
    stop: StopServer,
}

impl Tray for MaximaTray {
    fn title(&self) -> String {
        "Maxima server".into()
    }

    /// Themed icon name; degrades to the theme's fallback if absent.
    fn icon_name(&self) -> String {
        "application-x-executable".into()
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Open Maxima".into(),
                activate: Box::new(|_this: &mut Self| {
                    // The egui UI on Linux; it connects to the running server.
                    let _ = std::process::Command::new("maxima").spawn();
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Stop Server".into(),
                activate: Box::new(|this: &mut Self| (this.stop)()),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Start the SNI tray on a background thread. Best-effort.
pub fn spawn_sni(stop: StopServer) {
    let service = TrayService::new(MaximaTray { stop });
    service.spawn();
    log::info!("Linux SNI status icon started.");
}
