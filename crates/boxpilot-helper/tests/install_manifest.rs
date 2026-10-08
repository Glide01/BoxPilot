//! The helper directory a release ships, checked with the helper's own
//! manifest parser (ADR 0006 rules 3 and 7).
//!
//! The Windows release job stages what the MSI installs into
//! `[ProgramFiles64Folder]BoxPilot\Helper` with
//! `packaging/windows/stage-helper.ps1`, then runs this with `--ignored` and
//! `BOXPILOT_HELPER_STAGE` set to that directory (absolute: tests run in the
//! crate's directory). A manifest the helper would refuse, a hash that
//! doesn't match, a file nobody accounts for, or one `wix/main.wxs` doesn't
//! install then fails the release instead of every user's TUN start.

use boxpilot_helper::manifest::{self, MANIFEST_FILE};
use std::collections::BTreeSet;
use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

/// The helper itself: beside its manifest, but not in it.
const HELPER_EXE: &str = "boxpilot-helper.exe";
/// The name the MSI installs sing-box under.
const SING_BOX_EXE: &str = "sing-box.exe";
/// The one file the MSI installs only when sing-box's archive ships it
/// (`HelperLibcronet` in `wix/main.wxs`).
const OPTIONAL: &str = "libcronet.dll";
/// How `wix/main.wxs` names a staged file in a `Source`.
const WXS_SOURCE: &str = r"$(var.CargoTargetBinDir)\helper\";

/// The file names `wix/main.wxs` installs from the helper staging directory.
fn wxs_sources() -> BTreeSet<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../wix/main.wxs");
    let wxs = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    wxs.match_indices(WXS_SOURCE)
        .map(|(at, _)| {
            let rest = &wxs[at + WXS_SOURCE.len()..];
            rest[..rest.find('\'').expect("a quoted Source")].to_owned()
        })
        .collect()
}

#[test]
#[ignore = "checks a staged release; CI runs it with BOXPILOT_HELPER_STAGE set"]
fn the_staged_helper_directory_is_what_the_helper_accepts() {
    let dir = PathBuf::from(
        env::var_os("BOXPILOT_HELPER_STAGE")
            .expect("BOXPILOT_HELPER_STAGE names the staged helper directory"),
    );
    let bytes = fs::read(dir.join(MANIFEST_FILE)).unwrap();
    let manifest = manifest::parse(&bytes).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(manifest.sing_box.file, SING_BOX_EXE);
    if let Some(version) = env::var_os("BOXPILOT_HELPER_SINGBOX_VERSION") {
        assert_eq!(Some(manifest.sing_box.version.as_str()), version.to_str());
    }

    let mut expected = BTreeSet::from([HELPER_EXE.to_owned(), MANIFEST_FILE.to_owned()]);
    for (file, sha256) in manifest.files() {
        let path = dir.join(file);
        let reader = File::open(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        manifest::verify(file, sha256, reader).unwrap_or_else(|e| panic!("{e}"));
        expected.insert(file.to_owned());
    }
    let staged: BTreeSet<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(staged, expected, "the staged files are not the manifest's");

    // Every staged file is one the MSI installs, and the MSI installs no
    // other, but for the optional one when it isn't staged.
    let sources = wxs_sources();
    assert!(
        staged.is_subset(&sources),
        "{staged:?} not all in {sources:?}"
    );
    let unstaged: Vec<&String> = sources.difference(&staged).collect();
    assert!(
        unstaged.is_empty() || unstaged == [OPTIONAL],
        "wix/main.wxs installs {unstaged:?}, which is not staged"
    );
}
