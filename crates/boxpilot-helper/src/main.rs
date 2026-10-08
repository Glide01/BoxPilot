//! `boxpilot-helper`: BoxPilot's privileged helper (ADR 0006). Windows is
//! its first platform; elsewhere it only says so.

fn main() {
    eprintln!(
        "boxpilot-helper: the privileged helper's platform layer is not built yet (ADR 0006)"
    );
    std::process::exit(boxpilot_helper::exit::UNSUPPORTED_OS);
}
