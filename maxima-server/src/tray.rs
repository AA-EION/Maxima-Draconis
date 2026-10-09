//! Windows system-tray icon for the running Maxima server.
//!
//! Runs its own thread with a Win32 message loop (tray icons require one).
//!   * "Open Maxima"  → launches the UI (`maxima.exe` beside this binary).
//!   * "Stop Server"  → asks the server to shut down, like `maxima-cli server-stop`.
//!
//! macOS and Linux don't use this: macOS shows a SwiftUI `MenuBarExtra` in
//! Maxima.app, and Linux runs headless (systemd) — see CLAUDE.md.

#![cfg(windows)]

use std::ptr::null_mut;
use std::sync::OnceLock;

use log::warn;
use winapi::shared::minwindef::{LPARAM, LRESULT, UINT, WPARAM};
use winapi::shared::windef::HWND;
use winapi::um::libloaderapi::GetModuleHandleW;
use winapi::um::shellapi::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use winapi::um::winuser::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetCursorPos, GetMessageW, LoadIconW, MAKEINTRESOURCEW, PostMessageW, RegisterClassW,
    SetForegroundWindow, TrackPopupMenu, TranslateMessage, IDI_APPLICATION, MF_STRING, MSG,
    TPM_LEFTALIGN, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_DESTROY, WM_NULL, WM_RBUTTONUP,
    WNDCLASSW,
};

const TRAY_CALLBACK: UINT = WM_APP + 1;
const ID_OPEN: u16 = 1001;
const ID_STOP: u16 = 1002;

// Stashed for the WndProc, which has no user-data channel we bother wiring.
// A tray is a singleton per server process.
static STOP: OnceLock<crate::status_icon::StopServer> = OnceLock::new();

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Start the tray on a dedicated thread. Never blocks the caller.
pub fn spawn_tray(stop: crate::status_icon::StopServer) {
    let _ = STOP.set(stop);
    std::thread::spawn(|| unsafe { run() });
}

unsafe fn run() {
    let hinstance = GetModuleHandleW(null_mut());
    let class_name = wide("MaximaTrayWindow");

    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: null_mut(),
        hCursor: null_mut(),
        hbrBackground: null_mut(),
        lpszMenuName: null_mut(),
        lpszClassName: class_name.as_ptr(),
    };
    RegisterClassW(&wc);

    // Hidden message window (created but never shown).
    let hwnd = CreateWindowExW(
        0,
        class_name.as_ptr(),
        wide("Maxima").as_ptr(),
        0,
        0,
        0,
        0,
        0,
        null_mut(),
        null_mut(),
        hinstance,
        null_mut(),
    );
    if hwnd.is_null() {
        warn!("tray: failed to create window");
        return;
    }

    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = TRAY_CALLBACK;
    // Maxima's own logo (embedded by build.rs / winres as icon id 1), falling
    // back to the generic application icon if the resource isn't present.
    let mut icon = LoadIconW(hinstance, MAKEINTRESOURCEW(1));
    if icon.is_null() {
        icon = LoadIconW(null_mut(), IDI_APPLICATION);
    }
    nid.hIcon = icon;
    let tip = wide("Maxima server");
    for (i, ch) in tip.iter().enumerate().take(nid.szTip.len()) {
        nid.szTip[i] = *ch;
    }
    Shell_NotifyIconW(NIM_ADD, &mut nid);

    let mut msg: MSG = std::mem::zeroed();
    while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    Shell_NotifyIconW(NIM_DELETE, &mut nid);
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        TRAY_CALLBACK => {
            if lparam as UINT == WM_RBUTTONUP {
                show_menu(hwnd);
            }
            0
        }
        WM_COMMAND => {
            match (wparam & 0xffff) as u16 {
                ID_OPEN => open_ui(),
                ID_STOP => {
                    send_shutdown();
                    PostMessageW(hwnd, WM_DESTROY, 0, 0);
                }
                _ => {}
            }
            0
        }
        WM_DESTROY => {
            winapi::um::winuser::PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let menu = CreatePopupMenu();
    AppendMenuW(menu, MF_STRING, ID_OPEN as usize, wide("Open Maxima").as_ptr());
    AppendMenuW(menu, MF_STRING, ID_STOP as usize, wide("Stop Server").as_ptr());

    let mut pt = std::mem::zeroed();
    GetCursorPos(&mut pt);
    // Required incantation so the menu dismisses correctly from a tray icon.
    SetForegroundWindow(hwnd);
    TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        0,
        hwnd,
        null_mut(),
    );
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
}

/// Launch the egui UI (`maxima.exe`) sitting next to this binary.
fn open_ui() {
    if let Ok(exe) = std::env::current_exe() {
        let ui = exe.with_file_name("maxima.exe");
        let _ = std::process::Command::new(ui).spawn();
    }
}

fn send_shutdown() {
    if let Some(stop) = STOP.get() {
        stop();
    }
}
