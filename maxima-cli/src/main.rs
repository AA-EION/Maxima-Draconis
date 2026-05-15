use clap::{Parser, Subcommand};

use anyhow::{bail, Result};
use inquire::{Select, Text};
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use regex::Regex;

use std::{path::PathBuf, sync::Arc, time::Instant};

#[cfg(windows)]
use is_elevated::is_elevated;

#[cfg(windows)]
use maxima::{
    core::background_service::request_registry_setup,
    util::service::{is_service_running, is_service_valid, register_service_user, start_service},
};

use maxima::{
    content::{
        downloader::ZipDownloader,
        manager::{QueuedGame, QueuedGameBuilder},
        ContentService,
    },
    core::{
        auth::{
            context::AuthContext,
            login::{begin_oauth_login_flow, manual_login},
            nucleus_auth_exchange, nucleus_token_exchange, TokenResponse,
        },
        clients::JUNO_PC_CLIENT_ID,
        cloudsync::CloudSyncLockMode,
        launch::{self, LaunchMode, LaunchOptions},
        library::OwnedTitle,
        manifest::{self, MANIFEST_RELATIVE_PATH},
        service_layer::{
            ServiceGetBasicPlayerRequestBuilder, ServiceGetLegacyCatalogDefsRequestBuilder,
            ServiceLegacyOffer, ServicePlayer, SERVICE_REQUEST_GETBASICPLAYER,
            SERVICE_REQUEST_GETLEGACYCATALOGDEFS,
        },
        LockedMaxima, Maxima, MaximaEvent, MaximaOptionsBuilder,
    },
    ooa,
    rtm::client::BasicPresence,
    util::{log::init_logger_named, native::take_foreground_focus, registry::check_registry_validity},
};

lazy_static! {
    static ref MANUAL_LOGIN_PATTERN: Regex = Regex::new(r"^(.*):(.*)$").unwrap();
    // Matches a well-formed EA offer ID like "Origin.OFR.50.0002694"
    static ref EA_OFFER_ID_PATTERN: Regex = Regex::new(r"^Origin\.OFR\.\d+\.\d+$").unwrap();
    // Matches a Steam App ID emitted by `link2ea://launchgame/<id>?platform=steam`
    static ref STEAM_APP_ID_PATTERN: Regex = Regex::new(r"^\d{1,10}$").unwrap();
}

/// Hardcoded fallback table for EA-published games available on Steam.
///
/// When TF2 (and similar) is launched from inside Steam, the URL Steam emits
/// is `link2ea://launchgame/<steam_app_id>?platform=steam&theme=...`. Steam
/// does NOT spawn the game executable — it expects the link2ea handler (us)
/// to take over the launch entirely: auth + spawn the binary with the right
/// env vars so TF2 connects to our LSX server.
///
/// For Steam-only owners whose EA account is not linked, the EA library
/// lookup will fail to translate the Steam App ID to an Origin offer ID
/// AND won't know where the game is installed. This table provides both:
///   - the EA Origin offer ID to use for license/auth
///   - the relative path inside Steam's `steamapps/common/` to find the exe
///
/// Discovery process at runtime (`resolve_steam_install_path`):
///   1. Read `HKLM\SOFTWARE\(Wow6432Node\)Valve\Steam\InstallPath` for Steam root
///   2. Parse `<steam>\steamapps\libraryfolders.vdf` for additional libraries
///   3. Look for `<library>\steamapps\appmanifest_<app_id>.acf` and its `installdir`
///   4. Fall back to `<steam_root>\steamapps\common\<install_subdir>\<exe>` from this table
///
/// Extend as more EA-on-Steam titles are validated.
struct SteamGameEntry {
    steam_app_id: &'static str,
    origin_offer_id: &'static str,
    /// Directory name under `steamapps/common/`, e.g. "Titanfall2"
    install_subdir: &'static str,
    /// Game executable filename within the install dir, e.g. "Titanfall2.exe"
    exe_name: &'static str,
}

const STEAM_GAMES: &[SteamGameEntry] = &[
    SteamGameEntry {
        steam_app_id: "1237970",
        // Note: NOT Origin.OFR.50.0002694 — that's Apex Legends. TF2's real
        // offer ID is Origin.OFR.50.0001456, confirmed against a real EA
        // library dump ("titanfall-2 - Titanfall 2 - Origin.OFR.50.0001456").
        origin_offer_id: "Origin.OFR.50.0001456",
        install_subdir: "Titanfall2",
        exe_name: "Titanfall2.exe",
    },
];

fn lookup_steam_game(steam_app_id: &str) -> Option<&'static SteamGameEntry> {
    STEAM_GAMES.iter().find(|g| g.steam_app_id == steam_app_id)
}

/// Resolve where a given Steam game is installed on disk. Returns the full
/// path to the game executable (e.g. `C:\...\Steam\steamapps\common\Titanfall2\Titanfall2.exe`)
/// or `None` if the game isn't installed in any known Steam library.
///
/// Lookup order:
///   1. Steam install path from registry (`HKLM\SOFTWARE\WOW6432Node\Valve\Steam\InstallPath`
///      or `HKLM\SOFTWARE\Valve\Steam\InstallPath`)
///   2. Common default install locations as a last-resort
///   3. Parse libraryfolders.vdf to find additional Steam library folders
///   4. Verify the executable exists at `<library>\steamapps\common\<subdir>\<exe>`
#[cfg(windows)]
fn resolve_steam_install_path(game: &SteamGameEntry) -> Option<PathBuf> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    let mut steam_roots: Vec<PathBuf> = Vec::new();

    // 1. Registry — try both views since Steam installs as 32-bit on most systems
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    for key in &[
        "SOFTWARE\\WOW6432Node\\Valve\\Steam",
        "SOFTWARE\\Valve\\Steam",
    ] {
        if let Ok(subkey) = hklm.open_subkey(key) {
            if let Ok(path) = subkey.get_value::<String, _>("InstallPath") {
                steam_roots.push(PathBuf::from(path));
            }
        }
    }

    // 2. Common defaults (covers fresh Wine bottles where the registry key
    //    may not have been written yet, or when running outside Wine)
    for default in &[
        "C:\\Program Files (x86)\\Steam",
        "C:\\Program Files\\Steam",
    ] {
        let p = PathBuf::from(default);
        if p.exists() && !steam_roots.contains(&p) {
            steam_roots.push(p);
        }
    }

    // 3. For each Steam root, gather library folders from libraryfolders.vdf
    //    and search for the game.
    for root in &steam_roots {
        let mut libraries: Vec<PathBuf> = vec![root.clone()];

        // Parse libraryfolders.vdf for extra library paths. VDF is a simple
        // key-value format; we don't need a full parser — just grep "path".
        let vdf_paths = [
            root.join("steamapps").join("libraryfolders.vdf"),
            root.join("config").join("libraryfolders.vdf"),
        ];
        for vdf in &vdf_paths {
            if let Ok(content) = std::fs::read_to_string(vdf) {
                for line in content.lines() {
                    let trimmed = line.trim();
                    // Lines look like:   "path"   "C:\\SteamLibrary"
                    if let Some(rest) = trimmed.strip_prefix("\"path\"") {
                        if let Some(start) = rest.find('"') {
                            let after = &rest[start + 1..];
                            if let Some(end) = after.find('"') {
                                let extra = PathBuf::from(after[..end].replace("\\\\", "\\"));
                                if !libraries.contains(&extra) {
                                    libraries.push(extra);
                                }
                            }
                        }
                    }
                }
            }
        }

        // 4. Verify the executable exists in each library
        for lib in &libraries {
            let exe = lib
                .join("steamapps")
                .join("common")
                .join(game.install_subdir)
                .join(game.exe_name);
            if exe.exists() {
                return Some(exe);
            }
        }
    }

    None
}

#[cfg(not(windows))]
fn resolve_steam_install_path(_game: &SteamGameEntry) -> Option<PathBuf> {
    None
}

#[derive(Subcommand, Debug)]
enum Mode {
    Launch {
        slug: String,

        #[arg(long)]
        game_path: Option<String>,

        #[arg(long)]
        game_args: Vec<String>,

        /// When set, offer_id must be a content ID, and the only authenticated
        /// requests are made to the license server. A dummy name will be used
        /// in place of your real username, and any online LSX requests will fail
        #[arg(long)]
        login: Option<String>,
    },
    ListGames,
    LocateGame {
        path: String,
    },
    CloudSync {
        game_slug: String,

        #[arg(long)]
        write: bool,
    },
    AccountInfo,
    CreateAuthCode {
        #[arg(long)]
        client_id: String,
    },
    JunoTokenRefresh,
    ReadLicenseFile {
        #[arg(long)]
        content_id: String,
    },
    GetUserById {
        #[arg(long)]
        user_id: String,
    },
    GetGameBySlug {
        #[arg(long)]
        slug: String,
    },
    TestRTMConnection,
    ListFriends,
    GetLegacyCatalogDef {
        #[arg(long)]
        offer_id: String,
    },
    DownloadSpecificFile {
        #[arg(long)]
        offer_id: String,

        #[arg(long)]
        build_id: String,

        #[arg(long)]
        file: String,
    },
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    mode: Option<Mode>,

    #[arg(long)]
    #[clap(global = true)]
    login: Option<String>,
}

/// Ensure a console window exists AND that Rust's stdio is wired up to it.
///
/// When `maxima-cli` is spawned by `maxima-bootstrap` (built as a Windows GUI
/// app via `#![windows_subsystem = "windows"]`), the child process inherits
/// the parent's stdio — which is null/invalid because bootstrap has no
/// console. Two things break:
///
/// 1. No console window appears at all (until we call `AllocConsole`).
/// 2. Even after `AllocConsole`, Rust's `println!` / `eprintln!` still write
///    to the invalid handles they inherited. `AllocConsole` does NOT
///    automatically redirect existing std handles — it only creates the
///    console window. We have to point `STD_OUTPUT_HANDLE` / `STD_ERROR_HANDLE`
///    / `STD_INPUT_HANDLE` at `CONOUT$` / `CONIN$` ourselves.
///
/// Without step 2 the v0.2.1 fix is decorative: the console window pops up
/// but stays blank because the logger writes go nowhere.
///
/// Idempotent: if a console is already attached (`cmd.exe` invocation),
/// `AllocConsole` fails harmlessly and we still rewire the std handles to
/// `CONOUT$` (which resolves to the existing console).
#[cfg(windows)]
fn ensure_console_attached() {
    use std::ptr::null_mut;
    use winapi::um::consoleapi::AllocConsole;
    use winapi::um::fileapi::{CreateFileA, OPEN_EXISTING};
    use winapi::um::handleapi::INVALID_HANDLE_VALUE;
    use winapi::um::processenv::SetStdHandle;
    use winapi::um::wincon::GetConsoleWindow;
    use winapi::um::winbase::{STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    use winapi::um::winnt::{FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ, GENERIC_WRITE};

    unsafe {
        if GetConsoleWindow().is_null() {
            // Failure here means we already had a console (rare given the null
            // check) or the OS refused to give us one; either way, file
            // logging still works as a fallback.
            AllocConsole();
        }

        // Rewire std handles to the (possibly freshly allocated) console.
        // Each `CreateFileA` opens an independent handle; passing the same
        // handle to multiple `SetStdHandle` calls is technically allowed but
        // fragile (closing one closes them all).
        let open_console = |name: &[u8]| -> *mut winapi::ctypes::c_void {
            CreateFileA(
                name.as_ptr() as *const i8,
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null_mut(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };

        let stdout = open_console(b"CONOUT$\0");
        if stdout != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_OUTPUT_HANDLE, stdout);
        }

        let stderr = open_console(b"CONOUT$\0");
        if stderr != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_ERROR_HANDLE, stderr);
        }

        let stdin = open_console(b"CONIN$\0");
        if stdin != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_INPUT_HANDLE, stdin);
        }
    }
}

#[cfg(not(windows))]
fn ensure_console_attached() {}

/// Install a panic hook that writes the panic message to a dedicated file
/// before unwinding. Without this, panics that happen *before* the regular
/// logger is initialized (or that hit `eprintln!` when stderr is unattached)
/// disappear silently — exactly the failure mode that made the v0.2.1
/// "nothing shows" bug so hard to diagnose.
///
/// File location matches the rest of the file logging:
///   - Windows: %LOCALAPPDATA%\Maxima\Logs\maxima-cli.panic.log
///   - Unix:    $XDG_DATA_HOME/maxima/logs/maxima-cli.panic.log (or ~/.local/share/...)
fn install_panic_hook() {
    let log_path: Option<std::path::PathBuf> = {
        #[cfg(windows)]
        {
            std::env::var_os("LOCALAPPDATA")
                .or_else(|| std::env::var_os("APPDATA"))
                .map(std::path::PathBuf::from)
                .map(|p| p.join("Maxima").join("Logs").join("maxima-cli.panic.log"))
        }
        #[cfg(unix)]
        {
            std::env::var_os("XDG_DATA_HOME")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(|h| std::path::PathBuf::from(h).join(".local").join("share"))
                })
                .map(|p| p.join("maxima").join("logs").join("maxima-cli.panic.log"))
        }
    };

    std::panic::set_hook(Box::new(move |info| {
        // Best-effort: never let the panic hook itself panic.
        if let Some(ref path) = log_path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                use std::io::Write;
                let _ = writeln!(
                    file,
                    "\n===== PANIC at {:?} (pid={}) =====\n{}",
                    std::time::SystemTime::now(),
                    std::process::id(),
                    info
                );
                let _ = file.flush();
            }
        }
        // Also try stderr (works once stdio is reattached to the console).
        eprintln!("FATAL: {}", info);
    }));
}

/// Plain (non-tokio) `main`. The order is load-bearing:
///
/// 1. Panic hook BEFORE anything fallible so a panic in any subsequent step
///    is captured on disk.
/// 2. Console + stdio reattach BEFORE any println / clap output so error
///    messages reach the user.
/// 3. Logger init BEFORE `Args::parse()` so clap's exit-on-error path can
///    hit the file sink.
/// 4. Argument parsing.
/// 5. Tokio runtime constructed manually so a panic in runtime setup (e.g.
///    IOCP init under Wine) is caught by the panic hook above — `#[tokio::main]`
///    would construct the runtime *before* user code, defeating step 1.
fn main() {
    install_panic_hook();
    ensure_console_attached();
    init_logger_named("maxima-cli");

    let args = Args::parse();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("Failed to build tokio runtime: {}", e);
            std::process::exit(1);
        }
    };

    let result = runtime.block_on(startup(args));

    if let Some(e) = result.err() {
        match std::env::var("RUST_BACKTRACE") {
            Ok(_) => error!("{}:\n{}", e, e.backtrace().to_string()),
            Err(_) => error!("{}: {}", e, e.root_cause()),
        }
    }
}

#[cfg(windows)]
async fn native_setup() -> Result<()> {
    if !is_elevated() {
        if !is_service_valid()? {
            info!("Installing service...");
            register_service_user()?;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        if !is_service_running()? {
            info!("Starting service...");
            start_service().await?;
        }
    }

    if let Err(err) = check_registry_validity() {
        warn!("{}, fixing...", err);
        request_registry_setup().await?;
    }

    Ok(())
}

#[cfg(not(windows))]
async fn native_setup() -> Result<()> {
    use maxima::util::registry::set_up_registry;

    if let Err(err) = check_registry_validity() {
        warn!("{}, fixing...", err);
        set_up_registry()?;
    }

    Ok(())
}

pub async fn login_flow(login_override: Option<String>) -> Result<TokenResponse> {
    let mut auth_context = AuthContext::new()?;

    if let Some(access_token) = &login_override {
        let access_token = if let Some(captures) = MANUAL_LOGIN_PATTERN.captures(&access_token) {
            let persona = &captures[1];
            let password = &captures[2];

            let login_result = manual_login(persona, password).await;
            if login_result.is_err() {
                bail!("Login failed: {}", login_result.err().unwrap().to_string());
            }

            login_result.unwrap()
        } else {
            access_token.to_owned()
        };

        auth_context.set_access_token(&access_token);
        let code = nucleus_auth_exchange(&auth_context, JUNO_PC_CLIENT_ID, "code").await?;
        auth_context.set_code(&code);
    } else {
        begin_oauth_login_flow(&mut auth_context).await?
    };

    if auth_context.code().is_none() {
        bail!("Login failed!");
    }

    if login_override.is_none() {
        info!("Received login...");
    }

    let token_res = nucleus_token_exchange(&auth_context).await;
    if token_res.is_err() {
        bail!("Login failed: {}", token_res.err().unwrap().to_string());
    }

    Ok(token_res?)
}

async fn startup(args: Args) -> Result<()> {
    // Args parsing and logger initialization happen in `main()` so a clap
    // exit hits the file sink and the panic hook is already installed by
    // the time the runtime is built.

    info!("Starting Maxima...");

    native_setup().await?;

    let skip_login = {
        if let Some(Mode::Launch {
            game_path: _,
            game_args: _,
            slug: _,
            ref login,
        }) = args.mode
        {
            login.is_some()
        } else {
            false
        }
    };

    let options = MaximaOptionsBuilder::default()
        .load_auth_storage(!skip_login)
        .dummy_local_user(skip_login)
        .build()?;

    let maxima_arc = Maxima::new_with_options(options).await?;

    if !skip_login {
        let maxima = maxima_arc.lock().await;

        {
            let mut auth_storage = maxima.auth_storage().lock().await;
            let logged_in = auth_storage.logged_in().await?;
            if !logged_in || args.login.is_some() {
                info!("Logging in...");
                let token_res = login_flow(args.login).await?;
                auth_storage.add_account(&token_res).await?;
            }
        }

        let user = maxima.local_user().await?;

        info!(
            "Logged in as {}!",
            user.player().as_ref().unwrap().display_name()
        );
    }

    // Take back the focus since the browser and bootstrap will take it
    take_foreground_focus()?;

    if args.mode.is_none() {
        run_interactive(maxima_arc.clone()).await?;
        return Ok(());
    }

    let mode = args.mode.unwrap();
    match mode {
        Mode::Launch {
            slug,
            game_path,
            game_args,
            login,
        } => {
            let offer_id = if login.is_none() {
                let mut maxima = maxima_arc.lock().await;
                
                // First try standard slug
                let mut found_offer_id = None;
                if let Ok(Some(offer)) = maxima.mut_library().game_by_base_slug(&slug).await {
                    found_offer_id = Some(offer.offer_id().clone());
                }
                
                // Then try base offer
                if found_offer_id.is_none() {
                    if let Ok(Some(offer)) = maxima.mut_library().game_by_base_offer(&slug).await {
                        found_offer_id = Some(offer.offer_id().clone());
                    }
                }
                
                // If still not found, do an exhaustive search across all properties 
                // (useful for Steam App IDs or content IDs)
                if found_offer_id.is_none() {
                    if let Ok(games) = maxima.mut_library().games().await {
                        for game in games {
                            let base = game.base_offer();
                            if base.slug() == &slug || 
                               base.offer_id() == &slug || 
                               base.product().id() == &slug || 
                               base.product().origin_offer_id() == &slug || 
                               base.offer().content_id() == &slug ||
                               base.product().product().id() == &slug 
                            {
                                found_offer_id = Some(base.offer_id().clone());
                                break;
                            }
                        }
                    }
                }
                
                if let Some(id) = found_offer_id {
                    id
                } else if EA_OFFER_ID_PATTERN.is_match(&slug) {
                    // The EA library lookup failed (e.g. Steam-only owner whose TF2 is not
                    // linked to their EA account), but the slug is already a well-formed EA
                    // offer ID — pass it through and let EA's license server decide.
                    warn!(
                        "Offer '{}' not found in EA library; passing through directly. \
                         If this fails, link your Steam account at https://www.ea.com",
                        slug
                    );
                    slug.clone()
                } else if STEAM_APP_ID_PATTERN.is_match(&slug) {
                    // Slug is a Steam App ID. The exhaustive library lookup above
                    // should have matched it via product.id / offer.content_id for
                    // any user whose Steam and EA accounts are linked. If we got
                    // here the accounts are not linked, so fall back to the static
                    // STEAM_GAMES table — the EA license server only accepts
                    // Origin offer IDs, not Steam IDs, so a passthrough would
                    // just fail with a less helpful error.
                    if let Some(game) = lookup_steam_game(&slug) {
                        warn!(
                            "Steam App ID '{}' not in EA library (Steam/EA accounts not linked?); \
                             using hardcoded fallback offer ID '{}'. Link your accounts at \
                             https://www.ea.com to remove this warning.",
                            slug, game.origin_offer_id
                        );
                        game.origin_offer_id.to_string()
                    } else {
                        bail!(
                            "Steam App ID '{}' is not in this user's EA library and has no \
                             hardcoded fallback. Link your Steam and EA accounts at https://www.ea.com, \
                             or open an issue if this is an EA-published game on Steam that should be supported.",
                            slug
                        );
                    }
                } else {
                    bail!("No owned offer found for '{}'. If this is an EA offer ID, make sure your EA and Steam accounts are linked at https://www.ea.com", slug);
                }
            } else {
                slug.clone()
            };

            // If the slug was a Steam App ID, the game is installed under
            // Steam's library, not EA Desktop's. `launch::start_game` would
            // bail with `NotInstalled` because EA's metadata doesn't know
            // about the Steam install. Discover the actual location from
            // Steam's registry + libraryfolders.vdf and pass it as an
            // explicit game_path override.
            let resolved_game_path = if game_path.is_none() && STEAM_APP_ID_PATTERN.is_match(&slug) {
                lookup_steam_game(&slug)
                    .and_then(resolve_steam_install_path)
                    .and_then(|p| p.to_str().map(|s| s.to_owned()))
                    .map(|p| {
                        info!("Discovered Steam install for app {}: {}", slug, p);
                        p
                    })
                    .or_else(|| {
                        warn!(
                            "Could not auto-discover Steam install path for app {}. \
                             If this game is installed in a non-standard location, \
                             pass --game-path manually.",
                            slug
                        );
                        None
                    })
            } else {
                game_path
            };

            // Steam DRM stub in EA-on-Steam titles (notably TF2) exits with
            // code 100010 ("Steam not detected") if launched without the
            // SteamAppId / SteamGameId env vars set. EA Desktop's Link2EA.exe
            // sets these when it spawns the game; we have to do the same.
            // The env vars propagate from this process through bootstrap to
            // the actual game executable via Command::env inheritance.
            let is_steam_launch = STEAM_APP_ID_PATTERN.is_match(&slug);
            if is_steam_launch {
                info!("Setting Steam env vars (SteamAppId={}) for Steam-launched game", slug);
                std::env::set_var("SteamAppId", &slug);
                std::env::set_var("SteamGameId", &slug);
                // Steam also normally exports these — set defaults so anything
                // that polls the env directly doesn't see them unset.
                if std::env::var("SteamClientLaunch").is_err() {
                    std::env::set_var("SteamClientLaunch", "1");
                }
                if std::env::var("SteamPath").is_err() {
                    std::env::set_var("SteamPath", "C:\\Program Files (x86)\\Steam");
                }
            }

            // Auto-inject launch args known to be required for Wine/Steam
            // launches. These are the same defaults flightcore-ng uses
            // (see catornot/flightcore-ng wine_run.rs L23-31):
            //   -noOriginStartup : skip the Origin "starting" wait that hangs
            //                      forever in Wine since EA Desktop isn't present
            //   -multiple        : allow multiple game instances (avoids the
            //                      "another instance already running" check that
            //                      can fire when Steam + Maxima race the launch)
            // Only add them if the user didn't already specify them.
            let mut final_game_args = game_args;
            if is_steam_launch {
                let has_no_origin = final_game_args
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case("-noOriginStartup"));
                let has_multiple = final_game_args
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case("-multiple"));
                if !has_no_origin {
                    final_game_args.insert(0, "-noOriginStartup".to_string());
                }
                if !has_multiple {
                    final_game_args.insert(0, "-multiple".to_string());
                }
                info!("Auto-injected Steam launch args; final args: {:?}", final_game_args);
            }

            start_game(&offer_id, resolved_game_path, final_game_args, login, is_steam_launch, maxima_arc.clone()).await
        }
        Mode::ListGames => list_games(maxima_arc.clone()).await,
        Mode::LocateGame { path } => locate_game(maxima_arc.clone(), &path).await,
        Mode::CloudSync { game_slug, write } => {
            do_cloud_sync(maxima_arc.clone(), &game_slug, write).await
        }
        Mode::AccountInfo => print_account_info(maxima_arc.clone()).await,
        Mode::CreateAuthCode { client_id } => {
            create_auth_code(maxima_arc.clone(), &client_id).await
        }
        Mode::JunoTokenRefresh => juno_token_refresh(maxima_arc.clone()).await,
        Mode::ReadLicenseFile { content_id } => read_license_file(&content_id).await,
        Mode::ListFriends => list_friends(maxima_arc.clone()).await,
        Mode::GetUserById { user_id } => get_user_by_id(maxima_arc.clone(), &user_id).await,
        Mode::GetGameBySlug { slug } => get_game_by_slug(maxima_arc.clone(), &slug).await,
        Mode::TestRTMConnection => test_rtm_connection(maxima_arc.clone()).await,
        Mode::GetLegacyCatalogDef { offer_id } => {
            get_legacy_catalog_def(maxima_arc.clone(), &offer_id).await
        }
        Mode::DownloadSpecificFile {
            offer_id,
            build_id,
            file,
        } => download_specific_file(maxima_arc.clone(), &offer_id, &build_id, &file).await,
    }?;

    Ok(())
}

async fn run_interactive(maxima_arc: LockedMaxima) -> Result<()> {
    let launch_options = vec![
        "Launch Game",
        "Install Game",
        "List Builds",
        "List Games",
        "Account Info",
    ];
    let name = Select::new(
        "Welcome to Maxima! What would you like to do?",
        launch_options,
    )
    .prompt()?;

    match name {
        "Launch Game" => interactive_start_game(maxima_arc.clone()).await?,
        "Install Game" => interactive_install_game(maxima_arc.clone()).await?,
        "List Builds" => generate_download_links(maxima_arc.clone()).await?,
        "List Games" => list_games(maxima_arc.clone()).await?,
        "Account Info" => print_account_info(maxima_arc.clone()).await?,
        _ => bail!("Something went wrong."),
    }

    Ok(())
}

async fn interactive_start_game(maxima_arc: LockedMaxima) -> Result<()> {
    let offer_id = {
        let mut maxima = maxima_arc.lock().await;

        let mut owned_games = Vec::new();
        for game in maxima.mut_library().games().await? {
            if !game.base_offer().is_installed().await {
                continue;
            }

            owned_games.push(game);
        }

        let owned_games_strs = owned_games
            .iter()
            .map(|g| g.name())
            .collect::<Vec<String>>();

        let name = Select::new("What game would you like to play?", owned_games_strs).prompt()?;
        let game: &OwnedTitle = owned_games.iter().find(|g| g.name() == name).unwrap();
        game.base_offer().offer_id().to_owned()
    };

    start_game(&offer_id, None, Vec::new(), None, false, maxima_arc.clone()).await?;

    Ok(())
}

async fn interactive_install_game(maxima_arc: LockedMaxima) -> Result<()> {
    let mut maxima = maxima_arc.lock().await;

    let offer_id = {
        let mut owned_games = Vec::new();
        for game in maxima.mut_library().games().await? {
            if game.base_offer().is_installed().await {
                continue;
            }

            owned_games.push(game);
        }

        let owned_games_strs = owned_games
            .iter()
            .map(|g| g.name())
            .collect::<Vec<String>>();

        let name =
            Select::new("What game would you like to install?", owned_games_strs).prompt()?;
        let game = owned_games.iter().find(|g| g.name() == name).unwrap();
        game.base_offer().offer_id().to_owned()
    };

    let builds = maxima
        .content_manager()
        .service()
        .available_builds(&offer_id)
        .await?;
    let build = builds.live_build();
    if build.is_none() {
        bail!("Couldn't find a suitable game build");
    }

    let build = build.unwrap();
    info!("Installing game build {}", build.to_string());

    let path = PathBuf::from(
        Text::new("Where would you like to install the game? (must be an absolute path)")
            .prompt()?,
    );
    if !path.is_absolute() {
        error!("Path {:?} is not absolute.", path);
        return Ok(());
    }

    let game = QueuedGameBuilder::default()
        .offer_id(offer_id)
        .build_id(build.build_id().to_owned())
        .path(path.clone())
        .build()?;

    let start_time = Instant::now();
    maxima.content_manager().install_now(game).await?;

    drop(maxima);

    loop {
        let mut maxima = maxima_arc.lock().await;

        for event in maxima.consume_pending_events() {
            match event {
                MaximaEvent::ReceivedLSXRequest(_pid, _request) => (),
                _ => {}
            }
        }

        maxima.update().await;

        if let Some(downloader) = maxima.content_manager().current() {
            info!("Downloading: {}%/100%", downloader.percentage_done());
        } else {
            break;
        }

        drop(maxima);
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    let end_time = Instant::now();
    let elapsed_time = end_time - start_time;

    info!(
        "Download took {}.{}",
        elapsed_time.as_secs(),
        elapsed_time.subsec_millis()
    );

    Ok(())
}

async fn download_specific_file(
    maxima_arc: LockedMaxima,
    offer: &str,
    build_id: &str,
    file: &str,
) -> Result<()> {
    let maxima = maxima_arc.lock().await;

    let content_service = ContentService::new(maxima.auth_storage().clone());
    let builds = content_service.available_builds(offer).await?;
    let build = builds.build(build_id);
    if build.is_none() {
        bail!("Couldn't find the game build {}", build_id);
    }

    let build = build.unwrap();
    info!("Downloading file from game build {}", build.to_string());

    let url = content_service
        .download_url(offer, Some(&build.build_id()))
        .await?;

    debug!("URL: {}", url.url());

    let downloader = ZipDownloader::new("test-game", &url.url(), "C:/DownloadTest").await?;
    let num_of_entries = downloader.manifest().entries().len();
    info!("Entries: {}", num_of_entries);

    let entry = downloader
        .manifest()
        .entries()
        .iter()
        .find(|x| x.name() == file);
    if entry.is_none() {
        bail!("Couldn't find the file {}", file);
    }

    let ele = entry.unwrap();
    downloader.download_single_file(ele, None).await.unwrap();

    info!(
        "Downloaded file {} from game build {}",
        file,
        build.to_string()
    );
    Ok(())
}

async fn generate_download_links(maxima_arc: LockedMaxima) -> Result<()> {
    let mut maxima = maxima_arc.lock().await;

    let content_service = ContentService::new(maxima.auth_storage().clone());

    let owned_games = maxima.mut_library().games().await?;
    let owned_games_strs = owned_games
        .iter()
        .map(|g| g.name())
        .collect::<Vec<String>>();

    let name = Select::new(
        "What game would you like to list builds for?",
        owned_games_strs,
    )
    .prompt()?;
    let game = owned_games.iter().find(|g| g.name() == name).unwrap();

    info!("Working...");

    let builds = content_service
        .available_builds(&game.base_offer().offer_id())
        .await?;

    let mut strs = String::new();
    for build in builds.builds {
        let url = content_service
            .download_url(&game.base_offer().offer_id(), Some(&build.build_id()))
            .await;
        if url.is_err() {
            continue;
        }

        let url = url.unwrap();

        strs += &build.to_string();
        strs += ": ";
        strs += url.url();
        strs += "\n";
    }

    println!("{}", strs);
    Ok(())
}

async fn print_account_info(maxima_arc: LockedMaxima) -> Result<()> {
    let mut maxima = maxima_arc.lock().await;
    let user = maxima.local_user().await?;

    info!("Access Token: {}", maxima.access_token().await?);
    info!("PC Sign: {}", AuthContext::new()?.generate_pc_sign()?);

    let player = user.player().as_ref().unwrap();
    info!("Username: {}", player.unique_name());
    info!("User ID: {}", user.id());
    info!("Persona ID: {}", player.psd());
    Ok(())
}

async fn create_auth_code(maxima_arc: LockedMaxima, client_id: &str) -> Result<()> {
    let mut maxima = maxima_arc.lock().await;

    let mut context = AuthContext::new()?;
    context.set_access_token(&maxima.access_token().await?);

    let auth_code = nucleus_auth_exchange(&context, client_id, "code").await?;
    info!("Auth Code for {}: {}", client_id, auth_code);
    info!("Code verifier: {}", context.code_verifier());
    Ok(())
}

async fn juno_token_refresh(maxima_arc: LockedMaxima) -> Result<()> {
    let mut maxima = maxima_arc.lock().await;

    let mut context = AuthContext::new()?;
    context.set_access_token(&maxima.access_token().await?);

    context.add_scope("basic.identity");
    context.add_scope("basic.persona");
    context.add_scope("basic.entitlement");

    let code = nucleus_auth_exchange(&context, JUNO_PC_CLIENT_ID, "code").await?;
    context.set_code(&code);

    if context.code().is_none() {
        bail!("Login failed!");
    }

    let token_res = nucleus_token_exchange(&context).await;
    if token_res.is_err() {
        bail!("Login failed: {}", token_res.err().unwrap().to_string());
    }

    let token_res = token_res.unwrap();
    info!("Access Token: {}", token_res.access_token());
    info!("Refresh Token: {:?}", token_res.refresh_token());
    info!("Token Type: {}", token_res.token_type());
    Ok(())
}

async fn read_license_file(content_id: &str) -> Result<()> {
    let path = ooa::get_license_dir()?.join(format!("{}.dlf", content_id));
    let mut data = tokio::fs::read(path).await?;
    data.drain(0..65); // Signature

    let license = ooa::decrypt_license(data.as_slice())?;
    info!("License: {:?}", license);

    Ok(())
}

async fn list_friends(maxima_arc: LockedMaxima) -> Result<()> {
    let maxima = maxima_arc.lock().await;

    for ele in maxima.friends(0).await? {
        info!(
            "{} [ID: {}, Persona ID: {}]",
            ele.display_name(),
            ele.pd(),
            ele.psd()
        );
    }

    Ok(())
}

async fn get_user_by_id(maxima_arc: LockedMaxima, user_id: &str) -> Result<()> {
    let maxima = maxima_arc.lock().await;

    let player: ServicePlayer = maxima
        .service_layer()
        .request(
            SERVICE_REQUEST_GETBASICPLAYER,
            ServiceGetBasicPlayerRequestBuilder::default()
                .pd(user_id.to_string())
                .build()?,
        )
        .await?;

    info!("Name: {}", player.display_name());

    dbg!(player);
    Ok(())
}

async fn get_game_by_slug(maxima_arc: LockedMaxima, slug: &str) -> Result<()> {
    let mut maxima = maxima_arc.lock().await;

    match maxima.mut_library().game_by_base_slug(slug).await? {
        Some(game) => {
            info!("Slug:       {}", game.slug());
            info!("Offer ID:   {}", game.offer_id());
            info!("Content ID: {}", game.offer().content_id());
            info!("Display:    {}", game.offer().display_name());
            info!("Installed:  {}", game.is_installed().await);
        }
        None => {
            bail!("No game found for slug '{}'", slug);
        }
    }

    Ok(())
}

async fn test_rtm_connection(maxima_arc: LockedMaxima) -> Result<()> {
    let mut maxima = maxima_arc.lock().await;
    let friends = maxima.friends(0).await?;

    let rtm = maxima.rtm();
    rtm.login().await?;
    rtm.set_presence(BasicPresence::Online, "Test", "Origin.OFR.50.0002148")
        .await?;

    let players: Vec<String> = friends.iter().map(|f| f.id().to_owned()).collect();
    info!("Subscribed to {} players", players.len());

    rtm.subscribe(&players).await?;
    drop(maxima);

    loop {
        let mut maxima = maxima_arc.lock().await;
        maxima.rtm().heartbeat().await?;

        {
            let store = maxima.rtm().presence_store().lock().await;
            for entry in store.iter() {
                info!(
                    "{}/{} is {:?}: In {}",
                    friends
                        .iter()
                        .find(|x| x.id().to_owned() == *entry.0)
                        .unwrap()
                        .display_name(),
                    entry.0,
                    entry.1.basic(),
                    entry.1.status()
                );
            }
        }

        drop(maxima);

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

async fn get_legacy_catalog_def(maxima_arc: LockedMaxima, offer_id: &str) -> Result<()> {
    let maxima = maxima_arc.lock().await;
    let defs: Vec<ServiceLegacyOffer> = maxima
        .service_layer()
        .request(
            SERVICE_REQUEST_GETLEGACYCATALOGDEFS,
            ServiceGetLegacyCatalogDefsRequestBuilder::default()
                .offer_ids(vec![offer_id.to_owned()])
                .locale(maxima.locale().clone())
                .build()?,
        )
        .await?;

    info!("Content ID: {}", defs[0].content_id());
    Ok(())
}

async fn list_games(maxima_arc: LockedMaxima) -> Result<()> {
    let mut maxima = maxima_arc.lock().await;

    info!("Owned games:");
    let titles = maxima.mut_library().games().await?;

    for title in titles {
        info!(
            "{:<width$} - {:<width2$} - {:<width3$} - Installed: {}",
            title.base_offer().slug(),
            title.name(),
            title.base_offer().offer_id(),
            title.base_offer().is_installed().await,
            width = 35,
            width2 = 35,
            width3 = 25,
        );

        for game in title.extra_offers() {
            info!(
                "  {:<width$} - {:<width2$}",
                game.offer().display_name(),
                game.offer_id(),
                width = 55,
                width2 = 25
            );
        }
    }

    Ok(())
}

async fn locate_game(maxima_arc: LockedMaxima, path: &str) -> Result<()> {
    let path = PathBuf::from(path);
    let manifest = manifest::read(path.join(MANIFEST_RELATIVE_PATH)).await?;
    manifest.run_touchup(&path).await?;
    info!("Installed!");
    Ok(())
}

async fn do_cloud_sync(maxima_arc: LockedMaxima, game_slug: &str, write: bool) -> Result<()> {
    let mut maxima = maxima_arc.lock().await;
    let offer = maxima
        .mut_library()
        .game_by_base_slug(game_slug)
        .await?
        .unwrap()
        .clone();

    info!("Got offer");

    let lock = maxima
        .cloud_sync()
        .obtain_lock(
            &offer,
            if write {
                CloudSyncLockMode::Write
            } else {
                CloudSyncLockMode::Read
            },
        )
        .await?;
    let res = lock.sync_files().await;
    lock.release().await?;
    res?;

    info!("Done");

    Ok(())
}

async fn start_game(
    offer_id: &str,
    game_path_override: Option<String>,
    game_args: Vec<String>,
    login: Option<String>,
    steam_launch: bool,
    maxima_arc: LockedMaxima,
) -> Result<()> {
    {
        let mut maxima = maxima_arc.lock().await;
        maxima.start_lsx(maxima_arc.clone()).await?;

        if login.is_none() {
            maxima.rtm().login().await?;

            let friends = maxima.friends(0).await?;
            let players: Vec<String> = friends.iter().map(|f| f.id().to_owned()).collect();
            info!("Subscribed to {} players", players.len());

            maxima.rtm().subscribe(&players).await?;
        }
    }

    let launch_options = LaunchOptions {
        path_override: game_path_override,
        arguments: game_args,
        cloud_saves: true,
        steam_launch,
    };

    if login.is_none() {
        launch::start_game(
            maxima_arc.clone(),
            LaunchMode::Online(offer_id.to_owned()),
            launch_options,
        )
        .await?;
    } else if let Some(captures) = MANUAL_LOGIN_PATTERN.captures(&login.unwrap()) {
        let persona = &captures[1];
        let password = &captures[2];

        launch::start_game(
            maxima_arc.clone(),
            LaunchMode::OnlineOffline(offer_id.to_owned(), persona.to_owned(), password.to_owned()),
            launch_options,
        )
        .await?;
    }

    loop {
        let mut maxima = maxima_arc.lock().await;

        for event in maxima.consume_pending_events() {
            match event {
                MaximaEvent::ReceivedLSXRequest(_pid, _request) => (),
                _ => {}
            }
        }

        maxima.update().await;
        if maxima.playing().is_none() {
            break;
        }

        drop(maxima);
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    Ok(())
}
