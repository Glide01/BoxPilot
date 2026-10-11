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
//!
//! The macOS release job does the same for the payload in the BoxPilot.app
//! it built (`BOXPILOT_HELPER_MACOS_CONTENTS`), which
//! `packaging/macos/helper-install.sh` installs.
//!
//! One more check runs everywhere, unignored: the MSI creates the state
//! folder where the helper looks for it, with the DACL the helper requires.

use boxpilot_helper::manifest::{self, MANIFEST_FILE};
use boxpilot_helper::paths::{HELPER_DIR, PRODUCT_DIR, STATE_DIR, STATE_DIR_DACL};
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

/// `wix/main.wxs`.
fn wxs() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../wix/main.wxs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The file names `wix/main.wxs` installs from the helper staging directory.
fn wxs_sources() -> BTreeSet<String> {
    let wxs = wxs();
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

/// The names in `dir`.
fn names(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}

/// The macOS payload inside a built BoxPilot.app (ADR 0006 rule 7): what
/// `packaging/macos/helper-install.sh` installs. The release job builds the
/// DMG, mounts it, and runs this with `--ignored` and
/// `BOXPILOT_HELPER_MACOS_CONTENTS` set to the app's `Contents` directory.
/// A manifest the helper would refuse, a sing-box that isn't the app's own,
/// a file nobody accounts for, or scripts and a plist other than the
/// repository's then fail the release instead of every user's install.
#[test]
#[ignore = "checks a built BoxPilot.app; CI runs it with BOXPILOT_HELPER_MACOS_CONTENTS set"]
fn the_macos_payload_is_what_the_helper_accepts() {
    use boxpilot_protocol::endpoint::macos;
    let contents = PathBuf::from(
        env::var_os("BOXPILOT_HELPER_MACOS_CONTENTS")
            .expect("BOXPILOT_HELPER_MACOS_CONTENTS names BoxPilot.app/Contents"),
    );
    let payload = contents.join(macos::BUNDLE_PAYLOAD_DIR);
    let bytes = fs::read(payload.join(MANIFEST_FILE)).unwrap();
    let manifest = manifest::parse(&bytes).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(manifest.sing_box.file, "sing-box");
    assert!(
        manifest.extra_files.is_empty(),
        "the macOS sing-box loads nothing from beside itself"
    );
    if let Some(version) = env::var_os("BOXPILOT_HELPER_SINGBOX_VERSION") {
        assert_eq!(Some(manifest.sing_box.version.as_str()), version.to_str());
    }
    // The helper's sing-box is the app's own, byte for byte, as signed.
    let sing_box = contents.join(macos::BUNDLE_SING_BOX);
    let reader = File::open(&sing_box).unwrap_or_else(|e| panic!("{}: {e}", sing_box.display()));
    manifest::verify("sing-box", &manifest.sing_box.sha256, reader)
        .unwrap_or_else(|e| panic!("{e}"));

    let expected = BTreeSet::from(
        [
            MANIFEST_FILE,
            macos::PLIST_FILE,
            macos::INSTALL_SCRIPT,
            macos::UNINSTALL_SCRIPT,
        ]
        .map(str::to_owned),
    );
    assert_eq!(names(&payload), expected, "the payload's files");
    let executables =
        BTreeSet::from(["BoxPilot", "sing-box", "boxpilot-helper"].map(str::to_owned));
    assert_eq!(names(&contents.join("MacOS")), executables);
    assert!(contents.join(macos::BUNDLE_HELPER).is_file());

    // What the install runs as root is the repository's.
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/macos");
    for script in [macos::INSTALL_SCRIPT, macos::UNINSTALL_SCRIPT] {
        assert_eq!(
            fs::read(payload.join(script)).unwrap(),
            fs::read(repo.join(script)).unwrap(),
            "{script}"
        );
    }
    assert_eq!(
        fs::read_to_string(payload.join(macos::PLIST_FILE)).unwrap(),
        boxpilot_helper::launchd::plist()
    );
}

/// The state folder is `[ProgramFiles64Folder]BoxPilot\HelperState`, beside
/// `Helper` in the same `BoxPilot` folder (`paths::Layout::installed`), and
/// the MSI creates it owned by SYSTEM with `paths::STATE_DIR_DACL`, the DACL
/// the helper requires there. Nothing goes to ProgramData, where any user
/// could have created the folder, or a junction, first.
#[test]
fn the_msi_creates_the_state_folder_the_helper_expects() {
    let wxs = wxs();
    let position = |text: &str| {
        assert_eq!(wxs.matches(text).count(), 1, "{text} once in wix/main.wxs");
        wxs.find(text).unwrap()
    };
    let program_files = position("<Directory Id='ProgramFiles64Folder'");
    let product = position(&format!(
        "<Directory Id='HelperParentFolder' Name='{PRODUCT_DIR}'>"
    ));
    let helper = position(&format!(
        "<Directory Id='HelperFolder' Name='{HELPER_DIR}'>"
    ));
    let state = position(&format!(
        "<Directory Id='HelperStateFolder' Name='{STATE_DIR}'>"
    ));
    assert!(program_files < product && product < helper && helper < state);
    // `Helper` is closed before `HelperState` opens: siblings, not nested.
    let between = &wxs[helper..state];
    assert_eq!(
        between.matches("<Directory ").count(),
        between.matches("</Directory>").count(),
        "HelperState must not be inside Helper"
    );
    let sddl = format!("Sddl='O:SYG:SY{STATE_DIR_DACL}'");
    let id = "Id='HelperStateSecurity'";
    let security = position(id);
    assert!(
        wxs[security + id.len()..].trim_start().starts_with(&sddl),
        "the state folder's PermissionEx is {sddl}"
    );
    assert!(state < security);
    assert!(!wxs.contains("CommonAppDataFolder"));
}
