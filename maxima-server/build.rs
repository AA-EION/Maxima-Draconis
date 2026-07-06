use std::io;

use maxima_resources::maxima_windows_rc;

fn main() -> io::Result<()> {
    // Embeds logo.ico (icon id 1) + version metadata into maxima-server.exe;
    // the tray reads that icon. No-op on non-Windows targets.
    maxima_windows_rc("maximaserver", "Maxima Server")
}
