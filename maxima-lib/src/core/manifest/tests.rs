use super::{
    dip::DiPManifest, expand_args, lenient::split_args, parse, pre_dip::PreDiPManifest,
    ManifestError,
};
use std::path::PathBuf;

const NORMAL_DIP: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<DiPManifest version="4.0">
  <gameTitles>
    <gameTitle locale="en_US">Some Game</gameTitle>
  </gameTitles>
  <contentIDs>
    <contentID>12345</contentID>
  </contentIDs>
  <buildMetaData>
    <featureFlags allowMultipleInstances="false" autoUpdateEnabled="true"
      dynamicContentSupportEnabled="true" enableDifferentialUpdate="true"
      enableOriginInGameAPI="true" forceTouchupInstallerAfterUpdate="false"
      languageChangeSupportEnabled="true" treatUpdatesAsMandatory="false"
      useGameVersionFromManifest="true"/>
    <gameVersion version="1.0.0.5"/>
    <requirements osMinVersion="6.0" osReqs64Bit="true"/>
  </buildMetaData>
  <runtime>
    <launcher>
      <name locale="en_US">Some Game</name>
      <filePath>[HKEY_LOCAL_MACHINE\SOFTWARE\Studio\Game\Install Dir]game.exe</filePath>
      <executeElevated>false</executeElevated>
      <trial>false</trial>
    </launcher>
    <launcher>
      <name locale="en_US">Some Game Trial</name>
      <filePath>[HKEY_LOCAL_MACHINE\SOFTWARE\Studio\Game\Install Dir]game_trial.exe</filePath>
      <trial>true</trial>
    </launcher>
  </runtime>
  <touchup>
    <filePath>__Installer/touchup.exe</filePath>
    <parameters>-install -locale {locale} -installPath "{installLocation}"</parameters>
  </touchup>
</DiPManifest>"#;

#[test]
fn normal_dip_manifest_is_unchanged() {
    let m = DiPManifest::parse(NORMAL_DIP).unwrap();

    assert_eq!(m.version().as_deref(), Some("1.0.0.5"));
    assert!(m.buildMetaData().featureFlags().attr_autoUpdateEnabled);
    assert!(!m.buildMetaData().featureFlags().attr_allowMultipleInstances);
    assert!(m.buildMetaData().requirements().attr_osReqs64Bit);
    assert_eq!(
        m.execute_path(false).as_deref(),
        Some(r"[HKEY_LOCAL_MACHINE\SOFTWARE\Studio\Game\Install Dir]game.exe")
    );
    assert_eq!(
        m.execute_path(true).as_deref(),
        Some(r"[HKEY_LOCAL_MACHINE\SOFTWARE\Studio\Game\Install Dir]game_trial.exe")
    );
    assert_eq!(m.touchup().path(), "__Installer/touchup.exe");
    assert_eq!(m.title("en_US"), Some("Some Game"));
    assert_eq!(m.runtime().launcher()[0].execute_elevated(), &Some(false));

    // Dispatch picks DiP, not pre-DiP.
    let boxed = parse(NORMAL_DIP.as_bytes()).unwrap();
    assert_eq!(boxed.version().as_deref(), Some("1.0.0.5"));
    assert!(boxed.execute_path(false).is_some());
}

#[test]
fn dip_without_launchers() {
    let empty_runtime = NORMAL_DIP
        .split("<runtime>")
        .next()
        .unwrap()
        .to_owned()
        + "<runtime></runtime><touchup><filePath>t.exe</filePath><parameters></parameters></touchup></DiPManifest>";
    let m = DiPManifest::parse(&empty_runtime).unwrap();
    assert_eq!(m.version().as_deref(), Some("1.0.0.5"));
    assert_eq!(m.execute_path(false), None);

    let no_runtime = empty_runtime.replace("<runtime></runtime>", "");
    let m = DiPManifest::parse(&no_runtime).unwrap();
    assert_eq!(m.execute_path(false), None);
    assert!(parse(no_runtime.as_bytes()).is_ok());
}

#[test]
fn dip_without_touchup_or_game_version() {
    let xml = r#"<DiPManifest version="4.0">
        <buildMetaData><requirements osMinVersion="6.0" osReqs64Bit="false"/></buildMetaData>
        <runtime><launcher><filePath>g.exe</filePath></launcher></runtime>
    </DiPManifest>"#;
    let m = DiPManifest::parse(xml).unwrap();
    assert_eq!(m.version(), None);
    assert!(m.touchup().is_empty());
    assert_eq!(m.execute_path(false).as_deref(), Some("g.exe"));
}

#[test]
fn pre_dip_without_game_version() {
    let xml = r#"<game manifestVersion="1.0">
        <metadata>
          <localeInfo locale="en_US"><title>Jade Empire</title></localeInfo>
        </metadata>
        <executable>
          <filePath>__Installer/cleanup.exe</filePath>
          <parameters>-install "{installLocation}"</parameters>
        </executable>
    </game>"#;

    assert!(DiPManifest::parse(xml).is_err());
    let m = PreDiPManifest::parse(xml).unwrap();
    assert_eq!(m.version(), None);
    assert_eq!(m.title("en_US"), Some("Jade Empire"));

    let boxed = parse(xml.as_bytes()).unwrap();
    assert_eq!(boxed.version(), None);
    assert_eq!(boxed.execute_path(false), None);
}

#[test]
fn pre_dip_with_version_in_either_case() {
    for attr in ["gameVersion", "GameVersion"] {
        let xml = format!(
            r#"<game {attr}="1.2.3" manifestVersion="1.0">
                 <executable><filePath>c.exe</filePath></executable>
               </game>"#
        );
        let boxed = parse(xml.as_bytes()).unwrap();
        assert_eq!(boxed.version().as_deref(), Some("1.2.3"), "{attr}");
    }
}

#[test]
fn string_booleans() {
    let xml = r#"<DiPManifest>
        <buildMetaData>
          <featureFlags allowMultipleInstances="True" autoUpdateEnabled="1"
            dynamicContentSupportEnabled="YES" enableDifferentialUpdate="0"
            enableOriginInGameAPI="" treatUpdatesAsMandatory="no"/>
          <requirements osReqs64Bit="TRUE"/>
        </buildMetaData>
        <runtime>
          <launcher><filePath>game.exe</filePath><trial>0</trial><executeElevated>1</executeElevated></launcher>
          <launcher><filePath>trial.exe</filePath><trial>True</trial></launcher>
        </runtime>
    </DiPManifest>"#;
    let m = DiPManifest::parse(xml).unwrap();
    let flags = m.buildMetaData().featureFlags();

    assert!(flags.attr_allowMultipleInstances);
    assert!(flags.attr_autoUpdateEnabled);
    assert!(flags.attr_dynamicContentSupportEnabled);
    assert!(!flags.attr_enableDifferentialUpdate);
    assert!(!flags.attr_enableOriginInGameAPI);
    assert!(!flags.attr_treatUpdatesAsMandatory);
    assert!(!flags.attr_useGameVersionFromManifest);
    assert!(m.buildMetaData().requirements().attr_osReqs64Bit);

    assert_eq!(m.execute_path(false).as_deref(), Some("game.exe"));
    assert_eq!(m.execute_path(true).as_deref(), Some("trial.exe"));
    assert_eq!(m.runtime().launcher()[0].execute_elevated(), &Some(true));
    assert_eq!(m.runtime().launcher()[1].execute_elevated(), &None);
}

#[test]
fn quoted_touchup_args() {
    let params = r#"-install -locale {locale} -installPath "{installLocation}" /D="{installLocation}\data" -silent"#;
    let args = expand_args(params, "en_US", r"Z:\home\me\My Games\Foo");

    let expected: Vec<PathBuf> = [
        "-install",
        "-locale",
        "en_US",
        "-installPath",
        r"Z:\home\me\My Games\Foo",
        r"/D=Z:\home\me\My Games\Foo\data",
        "-silent",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    assert_eq!(args, expected);

    // An unquoted placeholder holding a path with spaces stays one argument.
    assert_eq!(
        expand_args("{installLocation}", "en_US", r"C:\A B"),
        [PathBuf::from(r"C:\A B")]
    );
    assert!(expand_args("", "en_US", "C:").is_empty());
    assert_eq!(split_args("a  b"), ["a", "b"]);
}

#[test]
fn locale_lists() {
    // Several titles/names, and a single one, plus another element between
    // them (quick-xml needs `overlapped-lists` for that).
    let xml = r#"<DiPManifest>
        <gameTitles>
          <gameTitle locale="de_DE">Spiel</gameTitle>
          <gameTitle locale="en_US">Game</gameTitle>
        </gameTitles>
        <buildMetaData><gameVersion version="2"/></buildMetaData>
        <runtime>
          <launcher>
            <name locale="de_DE">Spiel starten</name>
            <filePath>a.exe</filePath>
            <name locale="en_US">Start game</name>
          </launcher>
          <launcher><name>Plain</name><filePath>b.exe</filePath><trial>1</trial></launcher>
        </runtime>
    </DiPManifest>"#;
    let m = DiPManifest::parse(xml).unwrap();

    assert_eq!(m.title("de_DE"), Some("Spiel"));
    assert_eq!(m.title("fr_FR"), Some("Game"));
    let launchers = m.runtime().launcher();
    assert_eq!(launchers[0].name().len(), 2);
    assert_eq!(launchers[0].name()[1].value, "Start game");
    assert_eq!(launchers[1].name().len(), 1);
    assert_eq!(launchers[1].name()[0].locale, "");
    assert_eq!(m.execute_path(true).as_deref(), Some("b.exe"));

    let pre = r#"<game gameVersion="1">
        <metadata>
          <localeInfo locale="fr_FR"><title>Jeu</title></localeInfo>
        </metadata>
        <executable><filePath>x.exe</filePath></executable>
    </game>"#;
    let m = PreDiPManifest::parse(pre).unwrap();
    assert_eq!(m.title("de_DE"), Some("Jeu"));
}

#[test]
fn unsupported_manifest_reports_both_attempts() {
    match parse(b"<nothing/>") {
        Err(ManifestError::Unsupported { .. }) => {}
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[test]
fn utf16_and_bom() {
    let utf16: Vec<u8> = [0xFFu8, 0xFE]
        .into_iter()
        .chain(NORMAL_DIP.encode_utf16().flat_map(|u| u.to_le_bytes()))
        .collect();
    assert_eq!(parse(&utf16).unwrap().version().as_deref(), Some("1.0.0.5"));

    let mut utf8_bom = vec![0xEF, 0xBB, 0xBF];
    utf8_bom.extend_from_slice(NORMAL_DIP.as_bytes());
    assert_eq!(
        parse(&utf8_bom).unwrap().version().as_deref(),
        Some("1.0.0.5")
    );
}

#[tokio::test]
async fn missing_manifest_is_a_clear_error() {
    let dir = std::env::temp_dir().join(format!("maxima-manifest-test-{}", std::process::id()));
    let missing = dir.join("does-not-exist").join(super::MANIFEST_RELATIVE_PATH);

    match super::read(missing.clone()).await {
        Err(ManifestError::NotFound(path)) => assert!(path.ends_with("installerdata.xml")),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn manifest_is_found_with_different_case() {
    let dir = std::env::temp_dir().join(format!("maxima-manifest-case-{}", std::process::id()));
    let installer = dir.join("__installer");
    std::fs::create_dir_all(&installer).unwrap();
    std::fs::write(installer.join("InstallerData.xml"), NORMAL_DIP).unwrap();

    let result = super::read(dir.join(super::MANIFEST_RELATIVE_PATH)).await;
    std::fs::remove_dir_all(&dir).unwrap();

    assert_eq!(result.unwrap().version().as_deref(), Some("1.0.0.5"));
}
