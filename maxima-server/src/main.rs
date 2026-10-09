//! `maxima-server` — the one process that does everything.
//!
//! It owns the logged-in `maxima-lib` session (login, LSX, RTM, downloads,
//! launch, install, verify, …) and answers `maxima-proto` RPCs over loopback
//! TCP. Nothing interacts with it directly: the CLI, TUI and GUI are its only
//! clients, each talking to it exclusively over the proto. Frontends spawn it
//! on demand (or it starts at logon), and it stays up for every client.

mod server;
mod status_icon;
#[cfg(windows)]
mod tray;
#[cfg(all(target_os = "linux", feature = "linux-tray"))]
mod linux_tray;

use anyhow::{bail, Result};
use log::{error, info, warn};
use maxima::core::{
    auth::{context::AuthContext, login::begin_oauth_login_flow, nucleus_token_exchange},
    LockedMaxima, Maxima, MaximaOptionsBuilder,
};
use maxima::util::{log::init_logger_named, native::maxima_dir};
use maxima_proto::instance::{GuardError, InstanceGuard};

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
    // One server per installation context (this user, or this Wine prefix).
    // Taking the lock first means two frontends racing to spawn a server
    // can't end up with two servers or two logins.
    let guard = match InstanceGuard::acquire(&maxima_dir()?, env!("CARGO_PKG_VERSION")) {
        Ok(guard) => guard,
        Err(GuardError::AlreadyRunning(dir)) => {
            info!("A Maxima server is already running for {}; exiting", dir.display());
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };
    info!("Starting Maxima server (realm {})...", guard.info().realm);

    // Sync our sibling binaries into the stable App Support dir (macOS) so
    // launchd / game-spawned bootstrap / every frontend can find us at the
    // well-known path. No-op elsewhere and when already running from it.
    maxima::server_client::ensure_app_support_install();

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

    server::run_server(maxima_arc, guard).await
}

/// Log in — OAuth on first run (opens the browser via qrc://), cached refresh
/// token afterwards. The server owns login; frontends only wait for `ready`.
/// Runs while the control port is already serving, so it must not hold the
/// `Maxima` lock for the minutes a user can spend in the browser.
pub(crate) async fn log_in(maxima_arc: &LockedMaxima) -> Result<()> {
    let auth_storage = maxima_arc.lock().await.auth_storage().clone();
    let mut auth_storage = auth_storage.lock().await;
    if auth_storage.logged_in().await? {
        return Ok(());
    }
    info!("Logging in...");
    let mut ctx = AuthContext::new()?;
    begin_oauth_login_flow(&mut ctx).await?;
    if ctx.code().is_none() {
        bail!("Login failed!");
    }
    let token = nucleus_token_exchange(&ctx).await?;
    auth_storage.add_account(&token).await?;
    Ok(())
}
