//! CrossOver bottle management for macOS.
//!
//! Uses CodeWeavers' `cxbottle` CLI (ships with CrossOver, semi-documented in
//! their support KB) to create one bottle per game so end users never touch
//! wine prefixes by hand. Bottles created here land in CrossOver's normal
//! bottle directory and show up in the CrossOver UI like any other bottle.
//!
//! Selection precedence, everywhere wine is touched (`wine_prefix_dir()`):
//! 1. Explicit `MAXIMA_WINE_PREFIX` — power users / Draconis pointing at an
//!    existing bottle. Never overridden.
//! 2. A `Maxima-<slug>` bottle, created on demand by [`ensure_game_bottle`].

use std::path::{Path, PathBuf};

use log::info;
use tokio::process::Command;

use crate::util::native::{NativeError, WineError};

pub const CROSSOVER_SUPPORT: &str = "/Applications/CrossOver.app/Contents/SharedSupport/CrossOver";

pub fn crossover_installed() -> bool {
    Path::new(CROSSOVER_SUPPORT).join("bin/cxbottle").exists()
}

/// CrossOver's bottle directory. Honors the custom location users can set in
/// CrossOver's preferences (stored in its defaults domain); falls back to the
/// standard `~/Library/Application Support/CrossOver/Bottles`.
pub fn bottles_dir() -> Result<PathBuf, NativeError> {
    if let Ok(out) = std::process::Command::new("defaults")
        .args(["read", "com.codeweavers.CrossOver", "BottleDir"])
        .output()
    {
        if out.status.success() {
            let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !dir.is_empty() {
                return Ok(PathBuf::from(dir));
            }
        }
    }

    let home = std::env::var("HOME")
        .map_err(|_| NativeError::MissingEnvironmentVariable("HOME".to_string()))?;
    Ok(PathBuf::from(home).join("Library/Application Support/CrossOver/Bottles"))
}

/// Newest 64-bit Windows template this CrossOver install ships.
fn best_template() -> &'static str {
    for template in ["win11_64", "win10_64"] {
        if Path::new(CROSSOVER_SUPPORT)
            .join("share/crossover/bottle_templates")
            .join(template)
            .exists()
        {
            return template;
        }
    }
    // Present in every CrossOver release we care about; cxbottle gives a
    // clear error if not.
    "win10_64"
}

/// Ensure a bottle named `name` exists, creating it via `cxbottle --create`
/// when missing. Returns the bottle's wine-prefix path.
pub async fn ensure_bottle(name: &str) -> Result<PathBuf, NativeError> {
    let bottle = bottles_dir()?.join(name);
    if bottle.join("system.reg").exists() {
        return Ok(bottle);
    }

    if !crossover_installed() {
        return Err(NativeError::CrossOverMissing);
    }

    let template = best_template();
    info!(
        "Creating CrossOver bottle '{}' (template {}) — first-time setup takes a minute...",
        name, template
    );

    let output = Command::new(format!("{}/bin/cxbottle", CROSSOVER_SUPPORT))
        .args([
            "--bottle",
            name,
            "--create",
            "--template",
            template,
            "--description",
            "Created by Maxima",
        ])
        .output()
        .await?;

    if !output.status.success() {
        let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
        if !output.stderr.is_empty() {
            combined.push_str("\n[stderr] ");
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        return Err(NativeError::Wine(WineError::Command {
            output: combined,
            exit: output.status,
        }));
    }

    info!("Bottle '{}' created at {}", name, bottle.display());
    Ok(bottle)
}

/// Per-game bottle selection. Honors an explicit `MAXIMA_WINE_PREFIX`; else
/// creates/reuses a `Maxima-<slug>` bottle and exports it via that same env
/// var so the whole pipeline follows — license dir, regedit, and the spawned
/// bootstrap/game child processes all resolve the prefix through
/// `wine_prefix_dir()`, and children inherit the environment.
pub async fn ensure_game_bottle(slug: &str) -> Result<PathBuf, NativeError> {
    if let Ok(prefix) = std::env::var("MAXIMA_WINE_PREFIX") {
        return Ok(PathBuf::from(prefix));
    }

    let bottle = ensure_bottle(&format!("Maxima-{}", slug)).await?;
    std::env::set_var("MAXIMA_WINE_PREFIX", &bottle);
    Ok(bottle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips a real bottle create + reuse + delete via cxbottle.
    /// Ignored by default: needs CrossOver installed and takes ~a minute.
    /// Run: cargo test -p maxima-lib cx_bottle_roundtrip -- --ignored
    #[test]
    #[ignore]
    fn cx_bottle_roundtrip() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let name = "Maxima-cxtest-scratch";

        let bottle = rt.block_on(ensure_bottle(name)).unwrap();
        assert!(bottle.join("system.reg").exists());

        // Second call must be a cheap no-op returning the same path.
        let again = rt.block_on(ensure_bottle(name)).unwrap();
        assert_eq!(bottle, again);

        let status = std::process::Command::new(format!("{}/bin/cxbottle", CROSSOVER_SUPPORT))
            .args(["--bottle", name, "--delete", "--force"])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(!bottle.exists());
    }
}
