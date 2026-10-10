//! `boxpilot-helper`: BoxPilot's privileged helper (ADR 0006): a service on
//! Windows, a launchd daemon on macOS; elsewhere it only says so.

#[cfg(windows)]
fn main() {
    std::process::exit(boxpilot_helper::win::main());
}

#[cfg(target_os = "macos")]
fn main() {
    std::process::exit(boxpilot_helper::mac::main());
}

#[cfg(not(any(windows, target_os = "macos")))]
fn main() {
    eprintln!("boxpilot-helper: the privileged helper runs on Windows and macOS only (ADR 0006)");
    std::process::exit(boxpilot_helper::exit::UNSUPPORTED_OS);
}
