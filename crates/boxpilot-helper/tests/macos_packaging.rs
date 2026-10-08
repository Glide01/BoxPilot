//! What the macOS packaging ships agrees with the helper (ADR 0006 rule 7),
//! checked on every OS: the plist is `launchd::plist()`'s text, and the
//! install, uninstall and smoke scripts spell the paths, the label, the
//! payload's place in the bundle and the exit codes as
//! `boxpilot_protocol::endpoint` does. A script that disagreed would install
//! the helper where it doesn't look, or test something else.
//!
//! The DMG's own payload is checked by `install_manifest`'s ignored test,
//! once CI has built it.

use boxpilot_helper::launchd;
use boxpilot_protocol::endpoint::{exit, macos};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn packaging(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packaging/macos")
        .join(name)
}

fn read(name: &str) -> String {
    let path = packaging(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The script's `NAME='value'` assignments, one per line, as the scripts
/// write their constants.
fn constants(script: &str) -> BTreeMap<String, String> {
    script
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once("='")?;
            let value = value.strip_suffix('\'')?;
            let constant = !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
            (constant && !value.contains('\'')).then(|| (name.to_owned(), value.to_owned()))
        })
        .collect()
}

/// Every constant a script may spell, as the endpoint says it.
fn endpoint() -> BTreeMap<&'static str, String> {
    let helper_tools = Path::new(macos::HELPER_PATH)
        .parent()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    [
        ("LABEL", macos::LABEL.to_owned()),
        ("HELPER_PATH", macos::HELPER_PATH.to_owned()),
        ("HELPER_TOOLS_DIR", helper_tools),
        ("SUPPORT_DIR", macos::SUPPORT_DIR.to_owned()),
        ("BIN_DIR", macos::BIN_DIR.to_owned()),
        ("SING_BOX_PATH", macos::SING_BOX_PATH.to_owned()),
        ("MANIFEST_PATH", macos::MANIFEST_PATH.to_owned()),
        ("STATE_DIR", macos::STATE_DIR.to_owned()),
        ("OWNER_FILE", macos::OWNER_FILE.to_owned()),
        ("LOG_FILE", macos::LOG_FILE.to_owned()),
        ("PLIST_PATH", macos::PLIST_PATH.to_owned()),
        ("SOCKET_PATH", macos::SOCKET_PATH.to_owned()),
        ("BUNDLE_HELPER", macos::BUNDLE_HELPER.to_owned()),
        ("BUNDLE_SING_BOX", macos::BUNDLE_SING_BOX.to_owned()),
        ("BUNDLE_PAYLOAD_DIR", macos::BUNDLE_PAYLOAD_DIR.to_owned()),
        ("PLIST_FILE", macos::PLIST_FILE.to_owned()),
        ("INSTALL_SCRIPT", macos::INSTALL_SCRIPT.to_owned()),
        ("UNINSTALL_SCRIPT", macos::UNINSTALL_SCRIPT.to_owned()),
        ("EXIT_OK", exit::OK.to_string()),
        (
            "EXIT_HELPER_DIR_REFUSED",
            exit::HELPER_DIR_REFUSED.to_string(),
        ),
        (
            "EXIT_STATE_DIR_REFUSED",
            exit::STATE_DIR_REFUSED.to_string(),
        ),
        ("EXIT_MANIFEST_REFUSED", exit::MANIFEST_REFUSED.to_string()),
        ("EXIT_SOCKET_FAILED", exit::SOCKET_FAILED.to_string()),
    ]
    .into_iter()
    .collect()
}

/// `script` spells every one of `required`, and whatever else of the
/// endpoint's it spells, the endpoint's way.
fn check_script(name: &str, required: &[&str]) {
    let spelled = constants(&read(name));
    let endpoint = endpoint();
    for required in required {
        assert!(
            spelled.contains_key(*required),
            "{name} doesn't set {required}"
        );
    }
    for (constant, value) in &spelled {
        if let Some(expected) = endpoint.get(constant.as_str()) {
            assert_eq!(value, expected, "{name}: {constant}");
        }
    }
}

#[test]
fn the_plist_is_the_helpers() {
    assert_eq!(
        read(macos::PLIST_FILE),
        launchd::plist(),
        "packaging/macos/{} must be launchd::plist()'s text",
        macos::PLIST_FILE
    );
}

#[test]
fn the_install_script_installs_where_the_helper_looks() {
    check_script(
        macos::INSTALL_SCRIPT,
        &[
            "LABEL",
            "HELPER_PATH",
            "HELPER_TOOLS_DIR",
            "SUPPORT_DIR",
            "BIN_DIR",
            "SING_BOX_PATH",
            "MANIFEST_PATH",
            "STATE_DIR",
            "OWNER_FILE",
            "PLIST_PATH",
            "SOCKET_PATH",
            "BUNDLE_HELPER",
            "BUNDLE_SING_BOX",
            "BUNDLE_PAYLOAD_DIR",
            "PLIST_FILE",
        ],
    );
}

#[test]
fn the_uninstall_script_removes_what_was_installed() {
    check_script(
        macos::UNINSTALL_SCRIPT,
        &[
            "LABEL",
            "HELPER_PATH",
            "SUPPORT_DIR",
            "BIN_DIR",
            "STATE_DIR",
            "PLIST_PATH",
            "SOCKET_PATH",
        ],
    );
}

#[test]
fn the_smoke_test_checks_the_real_thing() {
    check_script(
        "helper-smoke.sh",
        &[
            "LABEL",
            "HELPER_PATH",
            "SUPPORT_DIR",
            "BIN_DIR",
            "SING_BOX_PATH",
            "MANIFEST_PATH",
            "STATE_DIR",
            "OWNER_FILE",
            "LOG_FILE",
            "PLIST_PATH",
            "SOCKET_PATH",
            "BUNDLE_PAYLOAD_DIR",
            "INSTALL_SCRIPT",
            "UNINSTALL_SCRIPT",
            "EXIT_OK",
            "EXIT_HELPER_DIR_REFUSED",
            "EXIT_STATE_DIR_REFUSED",
            "EXIT_MANIFEST_REFUSED",
        ],
    );
}

/// The scripts take their values as arguments only: nothing from the
/// environment, and no `eval`.
#[test]
fn the_scripts_read_nothing_but_their_arguments() {
    for name in [macos::INSTALL_SCRIPT, macos::UNINSTALL_SCRIPT] {
        let script = read(name);
        assert!(script.starts_with("#!/bin/sh\n"), "{name}");
        assert!(script.contains("\nset -eu\n"), "{name}");
        assert!(
            script.contains("\nPATH='/usr/bin:/bin:/usr/sbin:/sbin'\nexport PATH\n"),
            "{name}"
        );
        let code: String = script
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        for word in [
            "eval ", "source ", "${HOME", "$HOME", "$USER", "$TMPDIR", "sudo ",
        ] {
            assert!(!code.contains(word), "{name} uses {word:?}");
        }
    }
}
