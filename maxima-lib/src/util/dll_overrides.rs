//! `WINEDLLOVERRIDES` handling shared by every path that starts a game under Wine.

use std::env;

/// Built-in overrides applied to games run through Maxima's own Wine invocation.
pub const DEFAULT_WINE_DLL_OVERRIDES: &str =
    "CryptBase,bcrypt,dxgi,d3d11,d3d12,d3d12core=n,b;winemenubuilder.exe=d";

/// Environment variable whose value is appended to the built-in overrides.
pub const WINE_DLL_OVERRIDES_ENV: &str = "MAXIMA_WINE_DLL_OVERRIDES";

/// Merges `WINEDLLOVERRIDES`-style specs (`dll[,dll]=mode[;...]`). A DLL named
/// by a later spec replaces its earlier mode; DLLs sharing a mode are grouped
/// in order of first appearance. Malformed entries are skipped.
pub fn merge_dll_overrides<'a>(specs: impl IntoIterator<Item = &'a str>) -> String {
    let mut entries: Vec<(String, String)> = Vec::new();

    for spec in specs {
        for part in spec.split(';') {
            let Some((dlls, mode)) = part.split_once('=') else {
                continue;
            };
            let mode = mode.trim();
            for dll in dlls.split(',').map(str::trim).filter(|d| !d.is_empty()) {
                match entries.iter_mut().find(|(d, _)| d == dll) {
                    Some(entry) => entry.1 = mode.to_owned(),
                    None => entries.push((dll.to_owned(), mode.to_owned())),
                }
            }
        }
    }

    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for (dll, mode) in &entries {
        match groups.iter_mut().find(|(m, _)| m == mode) {
            Some((_, dlls)) => dlls.push(dll),
            None => groups.push((mode, vec![dll])),
        }
    }

    groups
        .iter()
        .map(|(mode, dlls)| format!("{}={}", dlls.join(","), mode))
        .collect::<Vec<_>>()
        .join(";")
}

/// The overrides for one game launch: built-ins, then
/// `MAXIMA_WINE_DLL_OVERRIDES`, then the per-launch specs.
pub fn resolve_wine_dll_overrides(extra: &[String]) -> String {
    let from_env = env::var(WINE_DLL_OVERRIDES_ENV).unwrap_or_default();
    merge_dll_overrides(
        [DEFAULT_WINE_DLL_OVERRIDES, from_env.as_str()]
            .into_iter()
            .chain(extra.iter().map(String::as_str)),
    )
}

/// Overrides the user asked for explicitly (environment plus per-launch
/// specs), without the built-ins. Empty when nothing was requested.
pub fn requested_wine_dll_overrides(extra: &[String]) -> String {
    let from_env = env::var(WINE_DLL_OVERRIDES_ENV).unwrap_or_default();
    merge_dll_overrides(
        [from_env.as_str()]
            .into_iter()
            .chain(extra.iter().map(String::as_str)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_stable_under_merge() {
        assert_eq!(
            merge_dll_overrides([DEFAULT_WINE_DLL_OVERRIDES]),
            DEFAULT_WINE_DLL_OVERRIDES
        );
    }

    #[test]
    fn appends_new_dll_to_matching_group() {
        assert_eq!(
            merge_dll_overrides([DEFAULT_WINE_DLL_OVERRIDES, "wsock32=n,b"]),
            "CryptBase,bcrypt,dxgi,d3d11,d3d12,d3d12core,wsock32=n,b;winemenubuilder.exe=d"
        );
    }

    #[test]
    fn later_spec_wins_for_the_same_dll() {
        assert_eq!(
            merge_dll_overrides(["a,b=n,b;c=d", "b=b"]),
            "a=n,b;b=b;c=d"
        );
        assert_eq!(merge_dll_overrides(["a=n", "a=d"]), "a=d");
    }

    #[test]
    fn skips_malformed_entries_and_blanks() {
        assert_eq!(merge_dll_overrides(["", "nomode", ";;", " x , y =n"]), "x,y=n");
        assert_eq!(merge_dll_overrides(std::iter::empty()), "");
    }

    #[test]
    fn resolve_layers_env_then_launch_specs() {
        std::env::set_var(WINE_DLL_OVERRIDES_ENV, "foo=n");
        let resolved = resolve_wine_dll_overrides(&["foo=b".to_owned(), "bar=n,b".to_owned()]);
        let requested = requested_wine_dll_overrides(&["bar=n,b".to_owned()]);
        std::env::remove_var(WINE_DLL_OVERRIDES_ENV);
        assert_eq!(
            resolved,
            "CryptBase,bcrypt,dxgi,d3d11,d3d12,d3d12core,bar=n,b;winemenubuilder.exe=d;foo=b"
        );
        assert_eq!(requested, "foo=n;bar=n,b");
    }
}
