//! Running installer helpers (EA "touchup" executables) that may need
//! administrator rights.
//!
//! Elevation goes through the interactive UAC consent prompt for the one
//! program that asks for it — never through a privileged background service.
//! A service endpoint that executes a caller-chosen binary as SYSTEM lets any
//! local process escalate, so Maxima deliberately has none.

use std::path::{Path, PathBuf};

use crate::util::native::NativeError;

/// Run `program` with `args`, wait for it, and return its exit code.
///
/// The program is started normally first. Only when Windows refuses with
/// `ERROR_ELEVATION_REQUIRED` (its manifest demands administrator rights) is it
/// relaunched through UAC (`ShellExecuteExW` with the `runas` verb), which shows
/// the user a consent prompt naming the program and its signer.
#[cfg(windows)]
pub async fn run_and_wait(program: &Path, args: &[PathBuf]) -> Result<i32, NativeError> {
    use winapi::shared::winerror::ERROR_ELEVATION_REQUIRED;

    match tokio::process::Command::new(program).args(args).spawn() {
        Ok(mut child) => {
            let status = child.wait().await?;
            return Ok(status.code().unwrap_or(-1));
        }
        Err(err) if err.raw_os_error() == Some(ERROR_ELEVATION_REQUIRED as i32) => {
            log::info!("{} requires administrator rights; requesting elevation", program.display());
        }
        Err(err) => return Err(err.into()),
    }

    let program = program.to_path_buf();
    let parameters = join_windows_args(args);
    tokio::task::spawn_blocking(move || runas_and_wait(&program, &parameters))
        .await
        .map_err(|err| NativeError::Io(std::io::Error::other(err)))?
}

#[cfg(windows)]
fn runas_and_wait(program: &Path, parameters: &str) -> Result<i32, NativeError> {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt};
    use winapi::{
        shared::winerror::ERROR_CANCELLED,
        um::{
            errhandlingapi::GetLastError,
            handleapi::CloseHandle,
            processthreadsapi::GetExitCodeProcess,
            shellapi::{
                ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
            },
            synchapi::WaitForSingleObject,
            winbase::INFINITE,
            winuser::SW_SHOWNORMAL,
        },
    };

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(Some(0)).collect()
    }

    let verb = wide(OsStr::new("runas"));
    let file = wide(program.as_os_str());
    let params = wide(OsStr::new(parameters));
    let dir = program.parent().map(|p| wide(p.as_os_str()));

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: params.as_ptr(),
        lpDirectory: dir.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()),
        nShow: SW_SHOWNORMAL,
        ..Default::default()
    };

    // SAFETY: every pointer in `info` refers to a NUL-terminated buffer that
    // outlives the call; hProcess is only used if ShellExecuteExW set it.
    unsafe {
        if ShellExecuteExW(&mut info) == 0 {
            if GetLastError() == ERROR_CANCELLED {
                return Err(NativeError::Elevation(program.display().to_string()));
            }
            return Err(NativeError::Io(std::io::Error::last_os_error()));
        }
        if info.hProcess.is_null() {
            // The shell handed the request to an existing process; there is
            // nothing to wait on.
            return Ok(0);
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        if ok == 0 {
            return Err(NativeError::Io(std::io::Error::last_os_error()));
        }
        Ok(code as i32)
    }
}

/// Join arguments into one Windows command line, quoting each so that
/// `CommandLineToArgvW` (and the MSVC CRT) splits it back into the same
/// arguments. `ShellExecuteExW` takes a single parameter string, unlike
/// `CreateProcess` wrappers that quote for us.
#[cfg(any(windows, test))]
fn join_windows_args(args: &[PathBuf]) -> String {
    args.iter()
        .map(|arg| quote_windows_arg(&arg.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(any(windows, test))]
fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        return arg.to_owned();
    }

    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                // Backslashes before a quote are escaped, then the quote itself.
                quoted.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            c => {
                quoted.extend(std::iter::repeat('\\').take(backslashes));
                quoted.push(c);
                backslashes = 0;
            }
        }
    }
    // Backslashes before the closing quote must be doubled.
    quoted.extend(std::iter::repeat('\\').take(backslashes * 2));
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_matches_commandlinetoargvw_rules() {
        let cases = [
            ("plain", "plain"),
            ("", "\"\""),
            ("with space", "\"with space\""),
            (r"C:\Program Files\Game\", r#""C:\Program Files\Game\\""#),
            (r#"say "hi""#, r#""say \"hi\"""#),
            (r#"a\"b"#, r#""a\\\"b""#),
            (r"C:\no\spaces", r"C:\no\spaces"),
        ];
        for (input, expected) in cases {
            assert_eq!(quote_windows_arg(input), expected, "input: {input}");
        }
    }

    #[test]
    fn joins_arguments_with_spaces() {
        let args = [PathBuf::from("-locale"), PathBuf::from("en_US"), PathBuf::from(r"C:\Games\My Game")];
        assert_eq!(
            join_windows_args(&args),
            r#"-locale en_US "C:\Games\My Game""#
        );
    }
}
