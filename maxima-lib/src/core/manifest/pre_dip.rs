#![allow(non_snake_case)]

use crate::core::manifest::{
    bytes_to_string, collect_touchup_args,
    ManifestError,
};
use derive_getters::Getters;
use serde::Deserialize;
use std::path::PathBuf;

macro_rules! predip_type {
    (
        $(#[$message_attr:meta])*
        $message_name:ident;
        attr {
            $(
                $(#[$attr_field_attr:meta])*
                $attr_field:ident: $attr_field_type:ty
            ),* $(,)?
        },
        data {
            $(
                $(#[$field_attr:meta])*
                $field:ident: $field_type:ty
            ),* $(,)?
        }
    ) => {
        paste::paste! {
            // Main struct definition
            $(#[$message_attr])*
            #[derive(Default, Debug, Clone, Deserialize, PartialEq, Getters)]
            #[serde(rename_all = "camelCase")]
            pub struct [<PreDiP $message_name>] {
                $(
                    $(#[$attr_field_attr])*
                    #[serde(rename = "@" $attr_field)]
                    pub [<attr_ $attr_field>]: $attr_field_type,
                )*
                $(
                    $(#[$field_attr])*
                    pub $field: $field_type,
                )*
            }
        }
    }
}

predip_type!(
    Executable;
    attr {},
    data {
        #[serde(default)]
        file_path: String,
        #[serde(default)]
        parameters: String,
    }
);

predip_type!(
    LocaleInfo;
    attr {
        #[serde(default)]
        locale: String,
    },
    data {
        #[serde(default)]
        title: String,
    }
);

predip_type!(
    Metadata;
    attr {},
    data {
        #[serde(default)]
        localeInfo: Vec<PreDiPLocaleInfo>,
    }
);

fn remove_leading_slash(path: &str) -> &str {
    path.strip_prefix('/').unwrap_or(path)
}

#[cfg(unix)]
fn remove_trailing_slash(path: &str) -> &str {
    path.strip_suffix('/').unwrap_or(path)
}

predip_type!(
    Manifest;
    attr {
        #[serde(default, alias = "@GameVersion")]
        gameVersion: String,
        #[serde(default, alias = "@ManifestVersion")]
        manifestVersion: String,
    },
    data {
        #[serde(default)]
        metadata: PreDiPMetadata,
        executable: PreDiPExecutable,
    }
);

impl PreDiPManifest {
    pub async fn read(path: &PathBuf) -> Result<Self, ManifestError> {
        let bytes = crate::core::manifest::read_bytes(path).await?;
        let string = bytes_to_string(&bytes).ok_or(ManifestError::Decode)?;

        Self::parse(&string)
    }

    /// `executable` is required: it is what tells a pre-DiP manifest apart
    /// from a DiP one. Everything else is optional.
    pub fn parse(string: &str) -> Result<Self, ManifestError> {
        Ok(quick_xml::de::from_str(string)?)
    }

    pub fn version(&self) -> Option<String> {
        let version = self.attr_gameVersion.trim();
        (!version.is_empty()).then(|| version.to_owned())
    }

    pub fn title(&self, locale: &str) -> Option<&str> {
        let infos = &self.metadata.localeInfo;
        infos
            .iter()
            .find(|l| l.attr_locale == locale)
            .or_else(|| infos.iter().find(|l| l.attr_locale == "en_US"))
            .or_else(|| infos.first())
            .map(|l| l.title.as_str())
    }

    #[cfg(unix)]
    pub async fn run_touchup(&self, install_path: &PathBuf) -> Result<(), ManifestError> {
        use log::warn;

        use crate::{
            core::launch::mx_linux_setup,
            unix::{
                fs::case_insensitive_path,
                wine::{
                    cleanup_interrupted_burn_installs, invalidate_mx_wine_registry,
                    run_wine_command, CommandType,
                },
            },
        };

        if self.executable.file_path.trim().is_empty() {
            return Ok(());
        }

        mx_linux_setup().await?;

        // Clear any interrupted WiX Burn installs (e.g. vcredist killed mid-run)
        // so they start fresh rather than trying to resume from a corrupt checkpoint.
        if let Err(err) = cleanup_interrupted_burn_installs().await {
            warn!("Burn cleanup check failed (proceeding with touchup anyway): {err:?}");
        }

        let install_path = PathBuf::from(remove_trailing_slash(
            install_path.to_str().ok_or(ManifestError::Decode)?,
        ));
        let args = collect_touchup_args(&self.executable.parameters, &install_path)?;

        let path = install_path.join(remove_leading_slash(&self.executable.file_path));
        let path = case_insensitive_path(path);
        run_wine_command(path, Some(args), None, true, CommandType::Run).await?;

        invalidate_mx_wine_registry().await;
        Ok(())
    }

    #[cfg(windows)]
    pub async fn run_touchup(&self, install_path: &PathBuf) -> Result<(), ManifestError> {
        use crate::util::{native::NativeError, registry::cleanup_interrupted_burn_installs};
        use tokio::process::Command;

        if self.executable.file_path.trim().is_empty() {
            return Ok(());
        }

        // Clear any interrupted WiX Burn installs (e.g. vcredist killed mid-run)
        // so they start fresh rather than trying to resume from a corrupt checkpoint.
        cleanup_interrupted_burn_installs();

        let args = collect_touchup_args(&self.executable.parameters, install_path)?;
        let path = install_path.join(remove_leading_slash(&self.executable.file_path));

        let mut binding = Command::new(path);
        let child = binding.args(args);

        let status = child.spawn()?.wait().await?;
        if !status.success() {
            return Err(ManifestError::Native(NativeError::Command(
                status.code().unwrap_or(0),
            )));
        }

        Ok(())
    }
}
