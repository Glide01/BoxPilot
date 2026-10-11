//! The note that lets a launch undo the system proxy BoxPilot set for a run
//! through the privileged helper, if BoxPilot ended without undoing it
//! (Windows: `privileged_helper::GUI_SETS_SYSTEM_PROXY`).
//!
//! There the helper's SYSTEM sing-box never sets the user's proxy: BoxPilot
//! sets it, as the user, while the run is up (ADR 0006, "System proxy"). If
//! BoxPilot crashes or is killed, the helper stops that sing-box as the
//! connection ends (rule 6), but only BoxPilot can clear the proxy, which is
//! left pointing at a port nothing listens on: every program that follows it
//! loses the network. So the port is noted in `<data dir>/system-proxy`
//! before the proxy is set, and the note goes once the proxy is cleared; the
//! next launch that finds it clears the proxy, if it still points at that
//! port, before anything starts (`ProcessSession::new`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const MARKER_FILENAME: &str = "system-proxy";

fn marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join(MARKER_FILENAME)
}

/// Note that the system proxy is about to point at BoxPilot's port `port`.
pub fn mark(data_dir: &Path, port: u16) -> io::Result<()> {
    fs::write(marker_path(data_dir), format!("{port}\n"))
}

/// The proxy is cleared, or was never set: no note.
pub fn unmark(data_dir: &Path) {
    let _ = fs::remove_file(marker_path(data_dir));
}

/// The port a note left behind names, if there is one.
pub fn marked_port(data_dir: &Path) -> Option<u16> {
    parse_port(&fs::read_to_string(marker_path(data_dir)).ok()?)
}

fn parse_port(text: &str) -> Option<u16> {
    text.trim().parse().ok().filter(|port| *port != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "boxpilot-proxy-marker-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_note_names_its_port_until_it_is_removed() {
        let dir = temp_dir("round-trip");
        assert_eq!(marked_port(&dir), None);
        mark(&dir, 7788).unwrap();
        assert_eq!(marked_port(&dir), Some(7788));
        mark(&dir, 7890).unwrap();
        assert_eq!(marked_port(&dir), Some(7890), "the latest run's port");
        unmark(&dir);
        assert_eq!(marked_port(&dir), None);
        unmark(&dir);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_real_port_counts() {
        assert_eq!(parse_port("7788\n"), Some(7788));
        assert_eq!(parse_port(" 1 "), Some(1));
        for text in ["", "0", "-1", "65536", "7788x", "port"] {
            assert_eq!(parse_port(text), None, "{text:?}");
        }
    }
}
