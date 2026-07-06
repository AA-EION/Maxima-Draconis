//! `maxima-server` — the one process that does everything.
//!
//! It owns the logged-in `maxima-lib` session (login, LSX, RTM, downloads,
//! launch, install, verify, …) and answers `maxima-proto` RPCs over loopback
//! TCP. Nothing interacts with it directly: the CLI, TUI and GUI are its only
//! clients, each talking to it exclusively over the proto. Frontends spawn it
//! on demand (or it starts at logon), and it stays up for every client.

mod server;
#[cfg(windows)]
mod tray;

use anyhow::{bail, Result};
use log::{error, info, warn};
use maxima::core::{
    auth::{context::AuthContext, login::begin_oauth_login_flow, nucleus_token_exchange},
    Maxima, MaximaOptionsBuilder,
};
use maxima::util::log::init_logger_named;

fn main() {
    // Logger before the runtime so early failures land in the file sink.
    init_logger_named("maxima-server");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    if let Err(err) = rt.block_on(run()) {
        error!("maxima-server exited with error: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    info!("Starting Maxima server...");

    // Host-side wine registry setup (best effort; unix only — Windows service
    // handles its own setup).
    #[cfg(not(windows))]
    {
        use maxima::util::registry::{check_registry_validity, set_up_registry};
        if let Err(err) = check_registry_validity() {
            warn!("{}, fixing...", err);
            if let Err(err) = set_up_registry() {
                warn!("registry setup failed (continuing): {}", err);
            }
        }
    }

    let options = MaximaOptionsBuilder::default()
        .load_auth_storage(true)
        .dummy_local_user(false)
        .build()?;
    let maxima_arc = Maxima::new_with_options(options).await?;

    // Log in — OAuth on first run (opens the browser via qrc://), cached
    // refresh token afterwards. This is why the frontends never handle login:
    // the server owns it.
    {
        let maxima = maxima_arc.lock().await;
        let mut auth_storage = maxima.auth_storage().lock().await;
        if !auth_storage.logged_in().await? {
            info!("Logging in...");
            let mut ctx = AuthContext::new()?;
            begin_oauth_login_flow(&mut ctx).await?;
            if ctx.code().is_none() {
                bail!("Login failed!");
            }
            let token = nucleus_token_exchange(&ctx).await?;
            auth_storage.add_account(&token).await?;
        }
    }

    if let Ok(user) = maxima_arc.lock().await.local_user().await {
        if let Some(player) = user.player().as_ref() {
            info!("Logged in as {}!", player.display_name());
        }
    }

    server::run_server(maxima_arc).await
}
