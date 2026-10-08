#![allow(non_snake_case)]

use std::path::PathBuf;

use crate::core::manifest::{
    bytes_to_string, collect_touchup_args,
    lenient::{lenient_bool, lenient_opt_bool, pick_localized, LocalizedText},
    ManifestError,
};
use derive_getters::Getters;
use serde::Deserialize;

macro_rules! dip_type {
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
            pub struct [<DiP $message_name>] {
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

dip_type!(
    Launcher;
    attr {
        #[serde(default)]
        uid: String,
    },
    data {
        #[serde(default)]
        name: Vec<LocalizedText>,
        #[serde(default)]
        file_path: String,
        #[serde(default)]
        parameters: Option<String>,
        #[serde(default, deserialize_with = "lenient_opt_bool")]
        execute_elevated: Option<bool>,
        #[serde(default, deserialize_with = "lenient_bool")]
        trial: bool,
    }
);

dip_type!(
    FeatureFlags;
    attr {
        #[serde(default, deserialize_with = "lenient_bool")]
        allowMultipleInstances: bool,
        #[serde(default, deserialize_with = "lenient_bool")]
        autoUpdateEnabled: bool,
        #[serde(default, deserialize_with = "lenient_bool")]
        dynamicContentSupportEnabled: bool,
        #[serde(default, deserialize_with = "lenient_bool")]
        enableDifferentialUpdate: bool,
        #[serde(default, deserialize_with = "lenient_bool")]
        enableOriginInGameAPI: bool,
        #[serde(default, deserialize_with = "lenient_bool")]
        forceTouchupInstallerAfterUpdate: bool,
        #[serde(default, deserialize_with = "lenient_bool")]
        languageChangeSupportEnabled: bool,
        #[serde(default, deserialize_with = "lenient_bool")]
        treatUpdatesAsMandatory: bool,
        #[serde(default, deserialize_with = "lenient_bool")]
        useGameVersionFromManifest: bool,
    },
    data {}
);

dip_type!(
    GameVersion;
    attr {
        #[serde(default)]
        version: String,
    },
    data {}
);

dip_type!(
    Requirements;
    attr {
        #[serde(default)]
        osMinVersion: String,
        #[serde(default, deserialize_with = "lenient_bool")]
        osReqs64Bit: bool,
    },
    data {}
);

dip_type!(
    BuildMetaData;
    attr {},
    data {
        #[serde(default)]
        featureFlags: DiPFeatureFlags,
        #[serde(default)]
        gameVersion: DiPGameVersion,
        #[serde(default)]
        requirements: DiPRequirements,
    }
);

dip_type!(
    Runtime;
    attr {},
    data {
        #[serde(default)]
        launcher: Vec<DiPLauncher>,
    }
);

dip_type!(
    Touchup;
    attr {},
    data {
        #[serde(default)]
        file_path: String,
        #[serde(default)]
        parameters: String,
    }
);

dip_type!(
    GameTitles;
    attr {},
    data {
        #[serde(default)]
        gameTitle: Vec<LocalizedText>,
    }
);

fn remove_leading_slash(path: &str) -> &str {
    path.strip_prefix('/').unwrap_or(path)
}

#[cfg(unix)]
fn remove_trailing_slash(path: &str) -> &str {
    path.strip_suffix('/').unwrap_or(path)
}

impl DiPTouchup {
    pub fn path(&self) -> &str {
        remove_leading_slash(&self.file_path)
    }

    pub fn is_empty(&self) -> bool {
        self.file_path.trim().is_empty()
    }
}

dip_type!(
    Manifest;
    attr {
        #[serde(default)]
        version: String,
    },
    data {
        buildMetaData: DiPBuildMetaData,
        #[serde(default)]
        gameTitles: DiPGameTitles,
        #[serde(default)]
        runtime: DiPRuntime,
        #[serde(default)]
        touchup: DiPTouchup,
    }
);

dip_type!(
    LegacyManifest;
    attr {},
    data {
        executable: DiPTouchup,
    }
);

impl DiPManifest {
    pub async fn read(path: &PathBuf) -> Result<Self, ManifestError> {
        let bytes = crate::core::manifest::read_bytes(path).await?;
        let string = bytes_to_string(&bytes).ok_or(ManifestError::Decode)?;

        Self::parse(&string)
    }

    /// `buildMetaData` is required: it is what tells a DiP manifest apart from
    /// a pre-DiP one. Everything else is optional.
    pub fn parse(string: &str) -> Result<Self, ManifestError> {
        Ok(quick_xml::de::from_str(string)?)
    }

    pub fn execute_path(&self, trial: bool) -> Option<String> {
        self.runtime
            .launcher
            .iter()
            .find(|l| l.trial == trial && !l.file_path.is_empty())
            .map(|l| l.file_path.clone())
    }

    pub fn version(&self) -> Option<String> {
        let version = self.buildMetaData.gameVersion.attr_version.trim();
        (!version.is_empty()).then(|| version.to_owned())
    }

    pub fn title(&self, locale: &str) -> Option<&str> {
        pick_localized(&self.gameTitles.gameTitle, locale)
    }

    #[cfg(unix)]
    pub async fn run_touchup(&self, install_path: &PathBuf) -> Result<(), ManifestError> {
        use crate::{
            core::launch::mx_linux_setup,
            unix::{
                fs::case_insensitive_path,
                wine::{invalidate_mx_wine_registry, run_wine_command, CommandType},
            },
        };

        if self.touchup.is_empty() {
            return Ok(());
        }

        mx_linux_setup().await?;

        let install_path = PathBuf::from(remove_trailing_slash(
            install_path.to_str().ok_or(ManifestError::Decode)?,
        ));
        let args = collect_touchup_args(&self.touchup.parameters, &install_path)?;
        let path = install_path.join(self.touchup.path());
        let path = case_insensitive_path(path);
        run_wine_command(path, Some(args), None, true, CommandType::Run).await?;

        invalidate_mx_wine_registry().await;
        Ok(())
    }

    #[cfg(windows)]
    pub async fn run_touchup(&self, install_path: &PathBuf) -> Result<(), ManifestError> {
        use crate::util::{elevation, native::NativeError};

        if self.touchup.is_empty() {
            return Ok(());
        }

        let args = collect_touchup_args(&self.touchup.parameters, install_path)?;
        let path = install_path.join(self.touchup.path());

        let code = elevation::run_and_wait(&path, &args).await?;
        if code != 0 {
            return Err(ManifestError::Native(NativeError::Command(code)));
        }

        Ok(())
    }
}
