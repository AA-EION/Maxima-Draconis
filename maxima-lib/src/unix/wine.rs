use std::{path::PathBuf, fs::{create_dir_all, File, self, remove_dir_all}, io::Read, process::{Command, Stdio, ExitStatus}, ffi::OsStr, env};

use anyhow::{Result, bail};
use flate2::read::GzDecoder;
use lazy_static::lazy_static;
use log::{info, warn};
use regex::Regex;
use serde::{Serialize, Deserialize};
use tar::Archive;
use xz2::read::XzDecoder;

use crate::util::{native::maxima_dir, github::{fetch_github_release, github_download_asset, fetch_github_releases, GithubRelease}};

lazy_static! {
    static ref UMU_PATTERN: Regex = Regex::new(r"wine-lutris-GE-Proton.*\.tar\.xz").unwrap();
}

const VERSION_FILE: &str = "dependency-versions.toml";

#[derive(Serialize, Deserialize, Default)]
struct Versions {
    wine: String,
    dxvk: String,
    vkd3d: String,
}

pub fn wine_prefix_dir() -> Result<PathBuf> {
    Ok(maxima_dir()?.join("pfx"))
}

fn versions() -> Result<Versions> {
    let file = maxima_dir()?.join(VERSION_FILE);
    if !file.exists() {
        return Ok(Versions::default());
    }

    let data = fs::read_to_string(file)?;
    Ok(toml::from_str(&data)?)
}

fn set_versions(versions: Versions) -> Result<()> {
    let file = maxima_dir()?.join(VERSION_FILE);
    fs::write(file, toml::to_string(&versions)?)?;
    Ok(())
}

pub fn check_wine_validity() -> Result<bool> {
    let version = versions()?.wine;

    let release = get_umu_release();
    if release.is_err() {
        if !version.is_empty() {
            warn!("Failed to check umu-launcher release, rate limited?");
            return Ok(true);
        }

        bail!("Failed to check umu-launcher release: {}", release.err().unwrap());
    }

    Ok(version == release.unwrap().tag_name)
}

fn get_umu_release() -> Result<GithubRelease> {
    let releases = fetch_github_releases("Open-Wine-Components", "umu-launcher")?;

    let mut release = None;
    for r in releases {
        if r.tag_name.ends_with("LoL") {
            continue;
        }

        release = Some(r);
        break;
    }

    if release.is_none() {
        bail!("Couldn't find suitable umu-launcher release");
    }

    Ok(release.unwrap())
}

pub fn umu_dir() -> Result<PathBuf> {
    let home = if let Ok(home) = env::var("XDG_DATA_HOME") {
        home
    } else if let Ok(home) = env::var("HOME") {
        format!("{}/.local/share/Steam/compatibilitytools.d", home)
    } else {
        bail!("You don't have a HOME environment variable set");
    };

    let path = PathBuf::from(format!("{}/UMU-Latest", home));
    create_dir_all(&path)?;
    Ok(path)
}

pub fn umu_run<I: IntoIterator<Item = T>, T: AsRef<OsStr>>(
    arg: T,
    args: Option<I>,
    want_output: bool,
) -> Result<String> {
    let path = maxima_dir()?.join("umu-launcher/bin/umu-run");

    // Create command with all necessary wine env variables
    let mut binding = Command::new(path);
    let mut child = binding
        .env("WINEPREFIX", wine_prefix_dir()?)
        .env("GAMEID", "umu-0") // TODO: proper ids
        .env("STORE", "ea")
        .arg(arg);

    if let Some(arguments) = args {
        child = child.args(arguments);
    }

    let status: ExitStatus;
    let mut output_str = String::new();

    if want_output {
        let output = child.stdout(Stdio::piped()).spawn()?.wait_with_output()?;
        output_str = String::from_utf8_lossy(&output.stdout).to_string();
        status = output.status;
    } else {
        status = child.spawn()?.wait()?;
    };

    if !status.success() {
        bail!("{}", status.code().unwrap());
    }

    Ok(output_str.to_string())
}

pub fn run_wine_command<I: IntoIterator<Item = T>, T: AsRef<OsStr>>(
    program: &str,
    arg: T,
    args: Option<I>,
    want_output: bool,
) -> Result<String> {
    let path = umu_dir()?.join("files/bin/wine");

    // Create command with all necessary wine env variables
    let mut binding = Command::new(path);
    let mut child = binding
        .env("WINEPREFIX", wine_prefix_dir()?)
        .env("WINEESYNC", "1")
        .arg(arg);

    if let Some(arguments) = args {
        child = child.args(arguments);
    }

    let status: ExitStatus;
    let mut output_str = String::new();

    if want_output {
        let output = child.stdout(Stdio::piped()).spawn()?.wait_with_output()?;
        output_str = String::from_utf8_lossy(&output.stdout).to_string();
        status = output.status;
    } else {
        status = child.spawn()?.wait()?;
    };

    if !status.success() {
        bail!("{}", status.code().unwrap());
    }

    Ok(output_str.to_string())
}

pub async fn install_umu() -> Result<()> {
    let release = get_umu_release()?;
    let asset = release.assets.iter().find(|x| UMU_PATTERN.captures(&x.name).is_some());
    if asset.is_none() {
        // TODO: umu-launcher doesnt have a release yet, so we are going to compile and install it
        // bail!("Failed to find umu-launcher asset! the name pattern might be outdated, please make an issue at https://github.com/ArmchairDevelopers/Maxima/issues.");

        let dir = maxima_dir()?.join("downloads");
        create_dir_all(&dir)?;

        let umu_dir = dir.join("umu-launcher");

        let umu_install_dir = maxima_dir()?.join("umu-launcher");

        // clone git repo
        Command::new("git").current_dir(&dir).arg("clone").arg("https://github.com/Open-Wine-Components/umu-launcher.git").spawn()?.wait()?;

        // init submodule
        Command::new("git").current_dir(&umu_dir).arg("submodule").arg("update").arg("--init").arg("--recursive").spawn()?.wait()?;

        // configure
        Command::new(umu_dir.join("configure.sh")).current_dir(&umu_dir).arg(format!("--prefix={}", umu_install_dir.display())).spawn()?.wait()?;

        // make install
        Command::new("make").current_dir(&umu_dir).arg(format!("PREFIX={}", umu_install_dir.display())).arg("install").spawn()?.wait()?;

        return Ok(())
    }

    let asset = asset.unwrap();

    let dir = maxima_dir()?.join("downloads");
    create_dir_all(&dir)?;

    let path = dir.join(&asset.name);
    github_download_asset(asset, &path)?;
    extract_umu(&path)?;

    let mut versions = versions()?;
    versions.wine = release.tag_name;
    set_versions(versions)?;

    Ok(())
}

fn extract_umu(archive_path: &PathBuf) -> Result<()> {
    info!("Extracting umu-launcher...");

    let dir = maxima_dir()?.join("umu-launcher");
    if dir.exists() {
        remove_dir_all(&dir)?;
    }

    create_dir_all(&dir)?;

    let archive_file = File::open(archive_path)?;
    let archive_decoder = XzDecoder::new(archive_file);
    let mut archive = Archive::new(archive_decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?;
        
        let destination_path = dir.join(entry_path.strip_prefix(entry_path.components().next().unwrap())?);
        if let Some(parent_dir) = destination_path.parent() {
            std::fs::create_dir_all(parent_dir)?;
        }

        entry.unpack(destination_path)?;
    }

    Ok(())
}

pub fn setup_wine_registry() -> Result<()> {
    // TODO: probably best to add this to https://github.com/Open-Wine-Components/umu-protonfixes
    // util.regedit_add('HKEY_LOCAL_MACHINE\\Software\\Electronic Arts\\EA Desktop', 'InstallSuccessful', 'REG_SZ', 'true', True)
    // util.regedit_add('HKEY_LOCAL_MACHINE\\Software\\Origin', 'ClientPath', 'REG_SZ', 'C:/Windows/System32/conhost.exe')

    run_wine_command("wine", "reg", Some(vec!["add", "HKLM\\Software\\Electronic Arts\\EA Desktop", "/v", "InstallSuccessful",  "/d", "true", "/f", "/reg:64"]), false)?;
    run_wine_command("wine", "reg", Some(vec!["add", "HKLM\\Software\\Origin", "/v", "ClientPath",  "/d", "C:/Windows/System32/conhost.exe", "/f", "/reg:32"]), false)?;

    Ok(())
}