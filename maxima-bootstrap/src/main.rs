#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//extern crate windows_service;

use std::env::current_exe;
use std::path::{Path, PathBuf};
use std::error::Error;
use std::string::FromUtf8Error;
use thiserror::Error;
use tokio::process::Command;

use base64::{engine::general_purpose, Engine};
use maxima::core::launch::BootstrapLaunchArgs;
use maxima::util::native::NativeError;
#[cfg(windows)]
use maxima::util::service::{is_service_valid, register_service};
use maxima::util::BackgroundServiceControlError;
use url::Url;

#[cfg(target_os = "macos")]
mod macos;

/// Validates that an offer_id is one of the safe identifier shapes we'll
/// forward to `maxima-cli launch`.
///
/// Two forms are accepted:
///
/// 1. **EA Origin offer id** — `Origin.OFR.<digits>.<digits>` (e.g.
///    `Origin.OFR.50.0002694`). Emitted by EA Desktop and by games launched
///    directly outside Steam.
/// 2. **Pure-numeric Steam App ID** — e.g. `1237970` (Titanfall 2 on Steam).
///    Emitted by EA-published games when launched from inside Steam, where
///    the URL looks like `link2ea://launchgame/1237970?platform=steam&theme=tf2`.
///    `maxima-cli`'s exhaustive library lookup resolves these against the
///    user's owned games (matching against `product.id`, `offer.content_id`,
///    etc., not just the slug).
///
/// This is a defense against command-line injection: protocol handler URLs
/// (`link2ea://`, `origin2://`) are attacker-controlled. Without validation,
/// an attacker could craft a URL like `link2ea://launchgame/--login=stolen_token`
/// and `maxima-cli` would interpret `--login` as a flag, bypassing OAuth.
/// Both accepted shapes start with either an ASCII letter or digit, so flag
/// injection is structurally impossible.
fn is_valid_ea_offer_id(s: &str) -> bool {
    is_valid_origin_offer_id(s) || is_valid_steam_app_id(s)
}

fn is_valid_origin_offer_id(s: &str) -> bool {
    let mut parts = s.split('.');
    if parts.next() != Some("Origin") {
        return false;
    }
    if parts.next() != Some("OFR") {
        return false;
    }
    let Some(major) = parts.next() else { return false };
    let Some(minor) = parts.next() else { return false };
    if parts.next().is_some() {
        return false;
    }
    !major.is_empty()
        && !minor.is_empty()
        && major.chars().all(|c| c.is_ascii_digit())
        && minor.chars().all(|c| c.is_ascii_digit())
}

fn is_valid_steam_app_id(s: &str) -> bool {
    // 1..=10 digits covers every Steam App ID issued (current max is ~3M).
    // Reject empty (covers `link2ea://launchgame/` with no segment) and
    // anything that includes a non-digit (defends against `12--login=x`).
    !s.is_empty() && s.len() <= 10 && s.chars().all(|c| c.is_ascii_digit())
}

#[derive(Error, Debug)]
pub(crate) enum RunError {
    #[error(transparent)]
    BackgroundService(#[from] BackgroundServiceControlError),
    #[error(transparent)]
    Base64(#[from] base64::DecodeError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Native(#[from] NativeError),
    #[error(transparent)]
    ParseUrl(#[from] url::ParseError),
    #[error(transparent)]
    ParseUtf8(#[from] FromUtf8Error),
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
}

#[cfg(not(target_os = "macos"))]
#[tokio::main]
async fn main() -> Result<(), RunError> {
    // Immediate entry log
    if let Ok(temp_dir) = std::env::var("TEMP").map(PathBuf::from).or_else(|_| Ok::<PathBuf, RunError>(std::env::temp_dir())) {
        let debug_log = temp_dir.join("maxima_execution.log");
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&debug_log) {
            use std::io::Write;
            let _ = writeln!(file, "BOOTSTRAP MAIN START at {:?} | Raw Args: {:?}", std::time::SystemTime::now(), std::env::args().collect::<Vec<_>>());
        }
    }

    let _ = handle_launch_args().await?;

    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() -> Result<()> {
    use cacao::appkit::App;

    use crate::macos::MaximaBootstrapApp;

    let handle = tokio::runtime::Handle::current();
    App::new(
        "dev.armchairdevelopers.MaximaBootstrap",
        MaximaBootstrapApp::new(handle),
    )
    .run();

    Ok(())
}

async fn handle_launch_args() -> Result<bool, RunError> {
    let mut args: Vec<String> = std::env::args().collect();
    args.remove(0);

    let result = run(&args).await;
    let str_result = result
        .as_ref()
        .map_err(|e| {
            let source = e.source();
            let error_str = if source.is_some() {
                source.unwrap().to_string()
            } else {
                e.to_string()
            };

            error_str
        })
        .err()
        .unwrap_or("Success".to_string());
        
    // Unconditional debug log to verify execution (APPEND)
    let temp_dir = std::env::temp_dir();
    let debug_log = temp_dir.join("maxima_execution.log");
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&debug_log) {
        use std::io::Write;
        let _ = writeln!(file, "Maxima Bootstrap executed at {:?}\nArgs: {:?}\nResult: {}\n---", std::time::SystemTime::now(), args, str_result);
    }

    if str_result != "Success" {
        let log_path = temp_dir.join("maxima_bootstrap_error.log");
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
            use std::io::Write;
            let _ = writeln!(file, "Maxima Bootstrap Error at {:?}: {}", std::time::SystemTime::now(), str_result);
        }
        
        // Try a very simple path as well
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open("C:\\maxima_debug_error.log") {
            use std::io::Write;
            let _ = writeln!(file, "Maxima Bootstrap Error at {:?}: {}", std::time::SystemTime::now(), str_result);
        }
    }

    if cfg!(debug_assertions) || std::env::var("MAXIMA_DEBUG").is_ok() {
        println!("Args: {:?}", &args);
        println!("Result: {}", str_result);

        // Pause terminal
        //std::io::Read::read(&mut std::io::stdin(), &mut [0]).unwrap();
    }

    result
}

#[cfg(windows)]
fn service_setup() -> Result<(), BackgroundServiceControlError> {
    if is_service_valid()? {
        return Ok(());
    }

    register_service()?;

    Ok(())
}

#[cfg(not(windows))]
fn service_setup() -> Result<(), BackgroundServiceControlError> {
    Ok(())
}

#[cfg(windows)]
async fn platform_launch(args: BootstrapLaunchArgs) -> Result<(), NativeError> {
    let mut binding = Command::new(&args.path);
    let child = binding.args(&args.args);

    let temp_dir = std::env::temp_dir();
    let debug_log = temp_dir.join("maxima_execution.log");
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&debug_log) {
        use std::io::Write;
        let _ = writeln!(file, "PLATFORM_LAUNCH: Executing {:?} with args {:?}", args.path, args.args);
    }

    let status = child.spawn()?.wait().await?;
    if !status.success() {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, format!("Game exited with code: {:?}", status.code())).into());
    }
    Ok(())
}

#[cfg(unix)]
async fn platform_launch(args: BootstrapLaunchArgs) -> Result<(), NativeError> {
    use maxima::unix::wine::run_wine_command;
    use maxima::unix::wine::CommandType;

    run_wine_command(
        args.path,
        Some(args.args),
        None,
        false,
        CommandType::WaitForExitAndRun,
    )
    .await?;

    Ok(())
}

async fn run(args: &[String]) -> Result<bool, RunError> {
    let len = args.len();
    if len == 1 {
        let arg = &args[0];

        if arg == "--noop" {
            return Ok(true);
        }

        if arg.starts_with("link2ea") {
            // link2ea://launchgame/<offer-id>?platform=<p>&theme=<t>
            // link2ea://resume/<offer-id>?...
            let url = Url::parse(arg)?;

            // The offer ID is the first path segment after the host/action
            let segments: Vec<&str> = url
                .path_segments()
                .map(|c| c.collect())
                .unwrap_or_default();

            if segments.is_empty() {
                return Ok(false);
            }

            // segments[0] is the offer ID (e.g. "Origin.OFR.50.0002694")
            let offer_id = segments[0];

            // SECURITY: refuse anything that doesn't match the EA offer ID shape.
            // A URL like link2ea://launchgame/--login=stolen_token would otherwise
            // inject a flag into the maxima-cli invocation below.
            if !is_valid_ea_offer_id(offer_id) {
                let temp_dir = std::env::temp_dir();
                let debug_log = temp_dir.join("maxima_execution.log");
                if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&debug_log) {
                    use std::io::Write;
                    let _ = writeln!(file, "REJECTED malformed link2ea offer_id: {:?}", offer_id);
                }
                return Ok(false);
            }

            let mut child = Command::new(current_exe()?.with_file_name("maxima-cli.exe"));

            // Forward environment variables from parent process
            if let Ok(port) = std::env::var("KYBER_INTERFACE_PORT") {
                child.env("KYBER_INTERFACE_PORT", port);
            }

            // Extract any command params from the query string
            if let Some(query) = url.query() {
                let params = querystring::querify(query);
                if let Some((_, cmd_params)) = params.iter().find(|(k, _)| *k == "cmdParams") {
                    child.env(
                        "MAXIMA_LAUNCH_ARGS",
                        urlencoding::decode(cmd_params)
                            .unwrap_or_default()
                            .into_owned()
                            .replace("\\\"", "\""),
                    );
                }
            }

            child.args(["launch", offer_id]);
            let status = child.spawn()?.wait().await?;

            // Propagate non-zero exits as errors so handle_launch_args logs
            // them to maxima_execution.log and maxima_bootstrap_error.log via
            // the existing centralized error-reporting path. Previously we
            // logged manually and still returned Ok(true), which made failures
            // look like successes in the log.
            if !status.success() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("maxima-cli (link2ea) exited non-zero: code={:?}", status.code()),
                )
                .into());
            }

            return Ok(true);
        }

        if arg.starts_with("origin2") {
            // origin2://game/launch?offerIds=<offer_id>&cmdParams=<encoded_args>&...
            let url = Url::parse(arg)?;
            let query = querystring::querify(url.query().unwrap_or_default());
            let offer_id = query
                .iter()
                .find(|(k, _)| *k == "offerIds")
                .map(|(_, v)| *v)
                .unwrap_or_default();

            // SECURITY: same validation as link2ea:// — offer_id comes from an
            // attacker-controlled URL and must not be allowed to start with `--`.
            if !is_valid_ea_offer_id(offer_id) {
                let temp_dir = std::env::temp_dir();
                let debug_log = temp_dir.join("maxima_execution.log");
                if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&debug_log) {
                    use std::io::Write;
                    let _ = writeln!(file, "REJECTED malformed origin2 offer_id: {:?}", offer_id);
                }
                return Ok(false);
            }

            let mut child = Command::new(current_exe()?.with_file_name("maxima-cli.exe"));

            // Forward optional cmdParams as launch args
            if let Some((_, cmd_params)) = query.iter().find(|(k, _)| *k == "cmdParams") {
                child.env(
                    "MAXIMA_LAUNCH_ARGS",
                    urlencoding::decode(cmd_params)?
                        .into_owned()
                        .replace("\\\"", "\""),
                );
            }

            // Forward KYBER port if present in parent environment
            if let Ok(port) = std::env::var("KYBER_INTERFACE_PORT") {
                child.env("KYBER_INTERFACE_PORT", port);
            }

            child.args(["launch", offer_id]);
            let status = child.spawn()?.wait().await?;

            if !status.success() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("maxima-cli (origin2) exited non-zero: code={:?}", status.code()),
                )
                .into());
            }

            return Ok(true);
        }

        if arg.starts_with("qrc") {
            // Guard against malformed qrc:// URLs — splitn(2, ...) gives at most two
            // segments, so we won't panic on inputs that lack the marker.
            let parts: Vec<&str> = arg.splitn(2, "login_successful.html?").collect();
            let Some(query) = parts.get(1) else {
                return Ok(false);
            };
            reqwest::get(format!("http://127.0.0.1:31033/auth?{}", query)).await?;

            return Ok(true);
        }

        return Ok(false);
    }

    if len > 1 {
        let command = &args[0];
        let handled = match command.as_str() {
            "launch" => {
                let decoded = general_purpose::STANDARD.decode(&args[1])?;
                let launch_args: BootstrapLaunchArgs = serde_json::from_slice(&decoded)?;
                platform_launch(launch_args).await?;

                true
            }
            _ => false,
        };
        return Ok(handled);
    }

    service_setup()?;

    Ok(false)
}
