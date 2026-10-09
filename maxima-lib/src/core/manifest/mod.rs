pub mod dip;
pub mod lenient;
pub mod pre_dip;

use crate::util::native::platform_path;
use dip::DiPManifest;
use lenient::split_args;
use pre_dip::PreDiPManifest;
use quick_xml::DeError;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ManifestError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Xml(#[from] DeError),
    #[error(transparent)]
    Native(#[from] crate::util::native::NativeError),
    #[error(transparent)]
    Registry(#[from] crate::util::registry::RegistryError),

    #[error("failed to decode DiPManifest. Weird encoding?")]
    Decode,
    #[error("Unsupported Manifest.\nDiP Attempt: `{dip_attempt:?}`\nPreDiP Attempt: `{pre_dip_attempt:?}`")]
    Unsupported {
        dip_attempt: Box<ManifestError>,
        pre_dip_attempt: Box<ManifestError>,
    },
    #[error("could not find install path for `{0}`")]
    NoInstallPath(String),
    #[error("no installer manifest found at `{}` (is this the game's install folder?)", .0.display())]
    NotFound(PathBuf),
}

pub const MANIFEST_RELATIVE_PATH: &str = "__Installer/installerdata.xml";

#[async_trait::async_trait]
pub trait GameManifest: Send + std::fmt::Debug {
    /// Run the game's installer touchup. `wine_prefix` is the Wine prefix
    /// (unix) the game lives in; `None` means the ambient prefix. Ignored on
    /// Windows.
    async fn run_touchup(
        &self,
        install_path: &PathBuf,
        wine_prefix: Option<&Path>,
    ) -> Result<(), ManifestError>;
    fn execute_path(&self, trial: bool) -> Option<String>;
    fn version(&self) -> Option<String>;
}
#[async_trait::async_trait]
impl GameManifest for DiPManifest {
    async fn run_touchup(
        &self,
        install_path: &PathBuf,
        wine_prefix: Option<&Path>,
    ) -> Result<(), ManifestError> {
        self.run_touchup(install_path, wine_prefix).await
    }

    fn execute_path(&self, trial: bool) -> Option<String> {
        self.execute_path(trial)
    }

    fn version(&self) -> Option<String> {
        self.version()
    }
}

#[async_trait::async_trait]
impl GameManifest for PreDiPManifest {
    async fn run_touchup(
        &self,
        install_path: &PathBuf,
        wine_prefix: Option<&Path>,
    ) -> Result<(), ManifestError> {
        self.run_touchup(install_path, wine_prefix).await
    }

    fn execute_path(&self, _: bool) -> Option<String> {
        None // pre-dip games don't have an exe field, most if not all just use info in the offer
    }

    fn version(&self) -> Option<String> {
        self.version()
    }
}

/// UTF-8, or the UTF-16 some EA tools write. A leading BOM is dropped.
pub(crate) fn bytes_to_string(bytes: &[u8]) -> Option<String> {
    let utf16 = |bytes: &[u8], from: fn([u8; 2]) -> u16| {
        String::from_utf16(&bytes.chunks_exact(2).map(|a| from([a[0], a[1]])).collect::<Vec<_>>())
            .ok()
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, u16::from_be_bytes);
    }

    let string = match std::str::from_utf8(bytes) {
        Ok(v) => v.to_owned(),
        Err(_) => {
            let u16_bytes: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|a| u16::from_ne_bytes([a[0], a[1]]))
                .collect();
            String::from_utf16(&u16_bytes).ok()?
        }
    };

    Some(string.trim_start_matches('\u{feff}').to_owned())
}

pub fn parse(bytes: &[u8]) -> Result<Box<dyn GameManifest>, ManifestError> {
    let string = bytes_to_string(bytes).ok_or(ManifestError::Decode)?;

    let dip_attempt = DiPManifest::parse(&string);
    if let Ok(manifest) = dip_attempt {
        return Ok(Box::new(manifest));
    }
    let pre_dip_attempt = PreDiPManifest::parse(&string);
    if let Ok(manifest) = pre_dip_attempt {
        return Ok(Box::new(manifest));
    }

    Err(ManifestError::Unsupported {
        dip_attempt: dip_attempt.unwrap_err().into(),
        pre_dip_attempt: pre_dip_attempt.unwrap_err().into(),
    })
}

/// Reads and parses a manifest. On case-sensitive filesystems the path is
/// matched case-insensitively (installs made on Windows often have
/// `__installer` or `InstallerData.xml`), and a missing file is reported as
/// [`ManifestError::NotFound`] instead of a bare OS error.
pub async fn read(path: PathBuf) -> Result<Box<dyn GameManifest>, ManifestError> {
    #[cfg(unix)]
    let path = crate::unix::fs::case_insensitive_path(path);

    let bytes = read_bytes(&path).await?;
    parse(&bytes)
}

pub(crate) async fn read_bytes(path: &Path) -> Result<Vec<u8>, ManifestError> {
    tokio::fs::read(path).await.map_err(|err| {
        if matches!(
            err.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        ) {
            ManifestError::NotFound(path.to_path_buf())
        } else {
            ManifestError::Io(err)
        }
    })
}

/// Substitutes the manifest placeholders into one parameter string, splitting
/// the *template* (so an install path containing spaces stays one argument
/// whether or not the manifest quoted the placeholder).
pub(crate) fn expand_args(parameters: &str, locale: &str, install_location: &str) -> Vec<PathBuf> {
    split_args(parameters)
        .into_iter()
        .map(|arg| {
            PathBuf::from(
                arg.replace("{locale}", locale)
                    .replace("{installLocation}", install_location),
            )
        })
        .collect()
}

pub(crate) fn collect_touchup_args(
    parameters: &str,
    install_path: &Path,
) -> Result<Vec<PathBuf>, ManifestError> {
    let install = install_path
        .to_str()
        .ok_or(ManifestError::Decode)?
        .replace('/', "\\");
    let install = install.strip_suffix('\\').unwrap_or(&install);
    let install = platform_path(install);

    Ok(expand_args(
        parameters,
        "en_US",
        install.to_str().ok_or(ManifestError::Decode)?,
    ))
}

#[cfg(test)]
mod tests;
