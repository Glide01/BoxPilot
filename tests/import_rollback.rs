//! Regression test for the phantom-profile URI-import bug: with no profiles,
//! Home shows the "Add subscription" empty card (keyed on
//! `settings.has_profiles()`); a `sing-box://` import whose fetch fails must
//! not dismiss it. The scenario needs a live gpui App (async fetch task +
//! entity graph), which can't run inside the test process on macOS, so the
//! harness bin runs headless as a subprocess and reports via exit code.

#[test]
fn failed_uri_import_rolls_back_to_empty_state() {
    let exe = env!("CARGO_BIN_EXE_import_rollback_harness");
    let out = std::process::Command::new(exe)
        .output()
        .expect("spawn import_rollback_harness");
    assert!(
        out.status.success(),
        "harness exited with {:?}:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
}
