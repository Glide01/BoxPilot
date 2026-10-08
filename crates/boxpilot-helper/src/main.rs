//! `boxpilot-helper`: BoxPilot's privileged helper (ADR 0006). Windows is
//! its first platform; elsewhere it only says so.

#[cfg(windows)]
fn main() {
    std::process::exit(boxpilot_helper::win::main());
}

#[cfg(not(windows))]
fn main() {
    eprintln!("boxpilot-helper: the privileged helper runs on Windows only for now (ADR 0006)");
    std::process::exit(boxpilot_helper::exit::UNSUPPORTED_OS);
}
