//! Linux desktop integration for the AppImage build: registers BoxPilot as
//! the handler for `sing-box://` and `boxpilot://` links.
//!
//! An AppImage is a single file the user runs from wherever they saved it,
//! so nothing installs a `.desktop` entry for it. Without one `xdg-open`
//! has no handler for our schemes and import links do nothing. So on every
//! start as an AppImage (the runtime sets `$APPIMAGE` to the image's path)
//! the primary instance writes `boxpilot.desktop` and the icon into the
//! user's data dir, and — only when the entry actually changed, e.g. on the
//! first run or after the image was moved — asks `xdg-mime` to make it the
//! default handler.
//!
//! Everything here is best effort: failures are logged and startup goes
//! on. A missing link handler is a lesser problem than an app that won't
//! open.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DESKTOP_FILE_NAME: &str = "boxpilot.desktop";
const ICON_BYTES: &[u8] = include_bytes!("../../assets/icon.png");
const SCHEMES: [&str; 2] = ["sing-box", "boxpilot"];

/// Run once at startup by the primary instance. A no-op unless running as
/// an AppImage. Works on a background thread so `xdg-mime` (a shell script
/// that can take a while) never delays the window.
pub fn register_if_appimage() {
    let Some(appimage) = std::env::var_os("APPIMAGE").filter(|v| !v.is_empty()) else {
        return;
    };
    let Some(data_dir) = dirs::data_dir() else {
        eprintln!("No user data dir; skipping deep-link handler registration.");
        return;
    };
    std::thread::spawn(move || {
        let appimage = PathBuf::from(appimage);
        match install_files(&appimage, &data_dir) {
            Ok(true) => register_handlers(&data_dir.join("applications")),
            Ok(false) => {}
            Err(e) => eprintln!("Failed to install the BoxPilot desktop entry: {e}"),
        }
    });
}

/// Write the `.desktop` file and the icon under `data_dir`, each only if its
/// content differs. Returns whether the `.desktop` file was (re)written.
fn install_files(appimage: &Path, data_dir: &Path) -> std::io::Result<bool> {
    // Desktop entries are UTF-8; a path that isn't can't be put in `Exec`.
    let Some(appimage) = appimage.to_str() else {
        return Err(std::io::Error::other(format!(
            "AppImage path is not valid UTF-8: {}",
            appimage.display()
        )));
    };

    let icon = data_dir.join("icons/hicolor/256x256/apps/boxpilot.png");
    if let Err(e) = write_if_changed(&icon, ICON_BYTES) {
        // The entry is still useful without its icon.
        eprintln!("Failed to install the BoxPilot icon: {e}");
    }

    let entry = data_dir.join("applications").join(DESKTOP_FILE_NAME);
    write_if_changed(&entry, desktop_entry(appimage).as_bytes())
}

fn write_if_changed(path: &Path, content: &[u8]) -> std::io::Result<bool> {
    if std::fs::read(path).is_ok_and(|existing| existing == content) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(true)
}

/// Make the entry the default for both schemes and refresh the desktop
/// database. Missing binaries and failures are ignored: on a desktop
/// without these tools there is nothing better to fall back to.
fn register_handlers(apps_dir: &Path) {
    for scheme in SCHEMES {
        let mime = format!("x-scheme-handler/{scheme}");
        run_quietly(Command::new("xdg-mime").args(["default", DESKTOP_FILE_NAME, &mime]));
    }
    run_quietly(Command::new("update-desktop-database").arg(apps_dir));
}

fn run_quietly(command: &mut Command) {
    let _ = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The `.desktop` file text for an AppImage at `appimage`. Pure, so the
/// quoting can be tested.
fn desktop_entry(appimage: &str) -> String {
    let mime_types: String = SCHEMES
        .iter()
        .map(|scheme| format!("x-scheme-handler/{scheme};"))
        .collect();
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=BoxPilot\n\
         Exec={} %u\n\
         Icon=boxpilot\n\
         Terminal=false\n\
         Categories=Network;\n\
         MimeType={mime_types}\n\
         StartupWMClass=boxpilot\n",
        exec_argument(appimage)
    )
}

/// Quote one `Exec` argument per the Desktop Entry spec. Three layers,
/// innermost first:
/// 1. inside double quotes, `"`, `` ` ``, `$` and `\` take a backslash;
/// 2. `%` is a field-code prefix everywhere in `Exec`, so a literal one is
///    `%%`;
/// 3. the whole value is a string, whose own escapes (`\\`, `\n`, …) are
///    undone *before* the quoting — so every backslash from step 1 doubles
///    again. That's why the spec spells a literal `$` as `\\$` and a
///    literal backslash as `\\\\`.
fn exec_argument(arg: &str) -> String {
    let mut quoted = String::from("\"");
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');

    let mut escaped = String::with_capacity(quoted.len());
    for c in quoted.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\r' => escaped.push_str("\\r"),
            _ => escaped.push(c),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec_line(entry: &str) -> &str {
        entry.lines().find(|l| l.starts_with("Exec=")).unwrap()
    }

    #[test]
    fn plain_path_is_quoted_with_url_field_code() {
        let entry = desktop_entry("/home/me/Apps/BoxPilot.AppImage");
        assert_eq!(
            exec_line(&entry),
            r#"Exec="/home/me/Apps/BoxPilot.AppImage" %u"#
        );
    }

    #[test]
    fn path_with_spaces_stays_one_argument() {
        assert_eq!(
            exec_argument("/home/me/My Apps/Box Pilot.AppImage"),
            r#""/home/me/My Apps/Box Pilot.AppImage""#
        );
    }

    #[test]
    fn reserved_characters_are_escaped_for_both_layers() {
        // Spec examples: a literal `$` is `\\$`, a literal `\` is `\\\\`.
        assert_eq!(exec_argument("/a$b"), r#""/a\\$b""#);
        assert_eq!(exec_argument(r"/a\b"), r#""/a\\\\b""#);
        assert_eq!(exec_argument(r#"/a"b"#), r#""/a\\"b""#);
        assert_eq!(exec_argument("/a`b"), r#""/a\\`b""#);
    }

    #[test]
    fn percent_is_not_read_as_a_field_code() {
        assert_eq!(exec_argument("/a%u.AppImage"), r#""/a%%u.AppImage""#);
    }

    #[test]
    fn mime_type_line_lists_both_schemes() {
        let entry = desktop_entry("/x");
        assert!(entry
            .lines()
            .any(|l| l == "MimeType=x-scheme-handler/sing-box;x-scheme-handler/boxpilot;"));
    }

    #[test]
    fn entry_has_the_expected_keys() {
        let entry = desktop_entry("/x");
        assert_eq!(
            entry,
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=BoxPilot\n\
             Exec=\"/x\" %u\n\
             Icon=boxpilot\n\
             Terminal=false\n\
             Categories=Network;\n\
             MimeType=x-scheme-handler/sing-box;x-scheme-handler/boxpilot;\n\
             StartupWMClass=boxpilot\n"
        );
    }

    #[test]
    fn install_writes_once_and_rewrites_on_move() {
        let dir =
            std::env::temp_dir().join(format!("boxpilot-desktop-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        assert!(install_files(Path::new("/opt/BoxPilot.AppImage"), &dir).unwrap());
        let icon = dir.join("icons/hicolor/256x256/apps/boxpilot.png");
        assert_eq!(std::fs::read(&icon).unwrap(), ICON_BYTES);
        let entry = dir.join("applications/boxpilot.desktop");
        assert!(std::fs::read_to_string(&entry)
            .unwrap()
            .contains("Exec=\"/opt/BoxPilot.AppImage\" %u"));

        // Same image: nothing to do, so no handler re-registration.
        assert!(!install_files(Path::new("/opt/BoxPilot.AppImage"), &dir).unwrap());
        // Moved image: the entry must follow it.
        assert!(install_files(Path::new("/srv/BoxPilot.AppImage"), &dir).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
