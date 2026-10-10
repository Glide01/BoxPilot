//! The install manifest: which sing-box the helper may run, by hash, and
//! the files that sit beside it (ADR 0006 rule 3).
//!
//! The release build writes `manifest.json` next to the helper, and the
//! MSI installs both into the admin-only helper directory:
//!
//! ```json
//! {
//!   "manifest_version": 1,
//!   "sing_box": {
//!     "file": "sing-box.exe",
//!     "version": "1.14.2",
//!     "sha256": "<64 lowercase hex digits>"
//!   },
//!   "extra_files": [
//!     {"file": "libcronet.dll", "sha256": "<64 lowercase hex digits>"}
//!   ]
//! }
//! ```
//!
//! - `sing_box` is the binary the helper runs, and what `hello` reports.
//! - `extra_files` are what sing-box loads from beside itself: the naive
//!   outbound's `libcronet.dll`. The application directory comes first in
//!   the DLL search order, so these are checked exactly like sing-box.
//!
//! Parsing is strict, as the protocol's messages are: objects only (never
//! the array form serde would also take), no unknown fields, no duplicate
//! keys, plain file names, lowercase hex. A manifest that is anything else
//! keeps the helper from running at all.

#![forbid(unsafe_code)]

use serde::de::value::MapAccessDeserializer;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, Read};
use std::marker::PhantomData;

/// The manifest's file name, beside the helper.
pub const MANIFEST_FILE: &str = "manifest.json";

/// The `manifest_version` this build reads.
pub const MANIFEST_VERSION: u32 = 1;

/// A larger manifest is refused unread: a real one is a few hundred bytes.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;

/// The most `extra_files` a manifest lists.
pub const MAX_EXTRA_FILES: usize = 16;

/// The longest file name or version string.
const MAX_NAME: usize = 64;

/// A parsed, checked manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub sing_box: SingBox,
    pub extra_files: Vec<FileEntry>,
}

/// The sing-box binary the helper runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingBox {
    pub file: String,
    pub version: String,
    pub sha256: String,
}

/// A file sing-box loads from beside itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub file: String,
    pub sha256: String,
}

impl Manifest {
    /// Every file the manifest names, sing-box first, with its hash.
    pub fn files(&self) -> impl Iterator<Item = (&str, &str)> {
        std::iter::once((self.sing_box.file.as_str(), self.sing_box.sha256.as_str())).chain(
            self.extra_files
                .iter()
                .map(|entry| (entry.file.as_str(), entry.sha256.as_str())),
        )
    }
}

/// Why a manifest was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    TooLarge {
        bytes: usize,
    },
    /// Not JSON of the manifest's shape: serde_json's message.
    Malformed(String),
    UnsupportedVersion(u32),
    TooManyExtraFiles(usize),
    BadFileName(String),
    DuplicateFile(String),
    BadVersion,
    BadHash {
        file: String,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::TooLarge { bytes } => write!(
                f,
                "the manifest is {bytes} bytes, over the {MAX_MANIFEST_BYTES}-byte limit"
            ),
            ManifestError::Malformed(message) => write!(f, "the manifest is malformed: {message}"),
            ManifestError::UnsupportedVersion(version) => {
                write!(f, "manifest version {version} is not {MANIFEST_VERSION}")
            }
            ManifestError::TooManyExtraFiles(count) => write!(
                f,
                "the manifest lists {count} extra files, over the limit of {MAX_EXTRA_FILES}"
            ),
            ManifestError::BadFileName(name) => {
                write!(f, "`{name}` is not a plain file name the helper takes")
            }
            ManifestError::DuplicateFile(name) => {
                write!(f, "the manifest names `{name}` twice")
            }
            ManifestError::BadVersion => f.write_str("the sing-box version is malformed"),
            ManifestError::BadHash { file } => {
                write!(f, "the SHA-256 of `{file}` is not 64 lowercase hex digits")
            }
        }
    }
}

impl std::error::Error for ManifestError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireManifest {
    manifest_version: u32,
    #[serde(deserialize_with = "map_only")]
    sing_box: WireSingBox,
    #[serde(deserialize_with = "seq_of_maps")]
    extra_files: Vec<WireFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSingBox {
    file: String,
    version: String,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFile {
    file: String,
    sha256: String,
}

/// Parse and check a manifest.
pub fn parse(bytes: &[u8]) -> Result<Manifest, ManifestError> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ManifestError::TooLarge { bytes: bytes.len() });
    }
    let wire: WireManifest = {
        let mut deserializer = serde_json::Deserializer::from_slice(bytes);
        let wire =
            map_only(&mut deserializer).map_err(|e| ManifestError::Malformed(e.to_string()))?;
        deserializer
            .end()
            .map_err(|e| ManifestError::Malformed(e.to_string()))?;
        wire
    };
    if wire.manifest_version != MANIFEST_VERSION {
        return Err(ManifestError::UnsupportedVersion(wire.manifest_version));
    }
    if wire.extra_files.len() > MAX_EXTRA_FILES {
        return Err(ManifestError::TooManyExtraFiles(wire.extra_files.len()));
    }
    let manifest = Manifest {
        sing_box: SingBox {
            file: wire.sing_box.file,
            version: wire.sing_box.version,
            sha256: wire.sing_box.sha256,
        },
        extra_files: wire
            .extra_files
            .into_iter()
            .map(|file| FileEntry {
                file: file.file,
                sha256: file.sha256,
            })
            .collect(),
    };
    if !is_version(&manifest.sing_box.version) {
        return Err(ManifestError::BadVersion);
    }
    // Windows file names are case-insensitive: `Sing-Box.exe` and
    // `sing-box.exe` are one file.
    let mut seen = BTreeSet::new();
    for (file, sha256) in manifest.files() {
        if !is_plain_file_name(file) || file.eq_ignore_ascii_case(MANIFEST_FILE) {
            return Err(ManifestError::BadFileName(file.to_owned()));
        }
        if !seen.insert(file.to_ascii_lowercase()) {
            return Err(ManifestError::DuplicateFile(file.to_owned()));
        }
        if !is_sha256_hex(sha256) {
            return Err(ManifestError::BadHash {
                file: file.to_owned(),
            });
        }
    }
    Ok(manifest)
}

/// Whether `name` is a file name the helper joins to its own directory:
/// 1 to 64 of `A-Z a-z 0-9 . _ -`, not starting or ending with a dot, no
/// `..`, and not a Windows device name (`CON`, `NUL`, `COM1`, …, with any
/// extension). So it can never name another directory, a stream (`:`), or a
/// device.
pub fn is_plain_file_name(name: &str) -> bool {
    if !(1..=MAX_NAME).contains(&name.len())
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        || name.starts_with('.')
        || name.ends_with('.')
        || name.contains("..")
    {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let device = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ((stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.len() == 4
        && stem.as_bytes()[3].is_ascii_digit());
    !device
}

fn is_version(version: &str) -> bool {
    (1..=MAX_NAME).contains(&version.len())
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
}

fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The SHA-256 of everything `reader` yields, as lowercase hex.
pub fn sha256_hex(mut reader: impl Read) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Why a file failed its check against the manifest.
#[derive(Debug)]
pub enum VerifyError {
    Read(io::Error),
    Mismatch { file: String },
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyError::Read(error) => write!(f, "could not be read: {error}"),
            VerifyError::Mismatch { file } => {
                write!(f, "`{file}` does not match the install manifest")
            }
        }
    }
}

impl std::error::Error for VerifyError {}

/// Check the bytes `reader` yields against the manifest's hash for `file`.
pub fn verify(file: &str, expected_sha256: &str, reader: impl Read) -> Result<(), VerifyError> {
    let actual = sha256_hex(reader).map_err(VerifyError::Read)?;
    if actual != expected_sha256 {
        return Err(VerifyError::Mismatch {
            file: file.to_owned(),
        });
    }
    Ok(())
}

/// `T` from a JSON object only: serde's derived structs also take an array
/// of their fields in order, which `deny_unknown_fields` doesn't reach. The
/// protocol crate's adapter, for the same reason (one spelling per value).
fn map_only<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    struct Object<T>(PhantomData<T>);

    impl<'de, T: Deserialize<'de>> Visitor<'de> for Object<T> {
        type Value = T;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a JSON object")
        }

        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
            T::deserialize(MapAccessDeserializer::new(map))
        }
    }

    deserializer.deserialize_map(Object(PhantomData))
}

fn seq_of_maps<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Vec<T>, D::Error> {
    struct MapOnly<T>(T);

    impl<'de, T: Deserialize<'de>> Deserialize<'de> for MapOnly<T> {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            map_only(deserializer).map(MapOnly)
        }
    }

    let items = Vec::<MapOnly<T>>::deserialize(deserializer)?;
    Ok(items.into_iter().map(|MapOnly(item)| item).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const HASH2: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn valid() -> serde_json::Value {
        json!({
            "manifest_version": 1,
            "sing_box": {"file": "sing-box.exe", "version": "1.14.2", "sha256": HASH},
            "extra_files": [{"file": "libcronet.dll", "sha256": HASH2}]
        })
    }

    fn parse_value(value: &serde_json::Value) -> Result<Manifest, ManifestError> {
        parse(value.to_string().as_bytes())
    }

    fn malformed(value: serde_json::Value) {
        assert!(
            matches!(parse_value(&value), Err(ManifestError::Malformed(_))),
            "{value}"
        );
    }

    #[test]
    fn a_valid_manifest_parses() {
        let manifest = parse_value(&valid()).unwrap();
        assert_eq!(
            manifest,
            Manifest {
                sing_box: SingBox {
                    file: "sing-box.exe".into(),
                    version: "1.14.2".into(),
                    sha256: HASH.into(),
                },
                extra_files: vec![FileEntry {
                    file: "libcronet.dll".into(),
                    sha256: HASH2.into(),
                }],
            }
        );
        assert_eq!(
            manifest.files().collect::<Vec<_>>(),
            [("sing-box.exe", HASH), ("libcronet.dll", HASH2)]
        );
        let mut none = valid();
        none["extra_files"] = json!([]);
        assert!(parse_value(&none).unwrap().extra_files.is_empty());
    }

    #[test]
    fn only_objects_with_known_fields_parse() {
        let mut unknown = valid();
        unknown["signature"] = json!("x");
        malformed(unknown);
        let mut unknown = valid();
        unknown["sing_box"]["path"] = json!("C:\\x");
        malformed(unknown);
        let mut missing = valid();
        missing.as_object_mut().unwrap().remove("extra_files");
        malformed(missing);
        malformed(json!([1, ["sing-box.exe", "1.14.2", HASH], []]));
        let mut array = valid();
        array["sing_box"] = json!(["sing-box.exe", "1.14.2", HASH]);
        malformed(array);
        let mut array = valid();
        array["extra_files"] = json!([["libcronet.dll", HASH2]]);
        malformed(array);
        assert!(matches!(
            parse(br#"{"manifest_version":1,"manifest_version":1}"#),
            Err(ManifestError::Malformed(_))
        ));
        assert!(matches!(
            parse(format!("{} {{}}", valid()).as_bytes()),
            Err(ManifestError::Malformed(_))
        ));
        assert!(matches!(parse(b"\xff"), Err(ManifestError::Malformed(_))));
    }

    #[test]
    fn the_size_and_version_are_checked() {
        assert_eq!(
            parse(&vec![b' '; MAX_MANIFEST_BYTES + 1]),
            Err(ManifestError::TooLarge {
                bytes: MAX_MANIFEST_BYTES + 1
            })
        );
        let mut v2 = valid();
        v2["manifest_version"] = json!(2);
        assert_eq!(parse_value(&v2), Err(ManifestError::UnsupportedVersion(2)));
        for bad in ["", "1.14.2 beta", "1.14.2\n", &"1".repeat(65)] {
            let mut manifest = valid();
            manifest["sing_box"]["version"] = json!(bad);
            assert_eq!(
                parse_value(&manifest),
                Err(ManifestError::BadVersion),
                "{bad:?}"
            );
        }
        let mut many = valid();
        many["extra_files"] = json!((0..17)
            .map(|n| json!({"file": format!("f{n}.dll"), "sha256": HASH}))
            .collect::<Vec<_>>());
        assert_eq!(
            parse_value(&many),
            Err(ManifestError::TooManyExtraFiles(17))
        );
    }

    #[test]
    fn file_names_stay_in_the_helper_directory() {
        for bad in [
            "",
            "..\\sing-box.exe",
            "../sing-box",
            "C:\\sing-box.exe",
            "dir\\sing-box.exe",
            "sing-box.exe:stream",
            ".hidden",
            "trailing.",
            "a..b",
            "sing box.exe",
            "NUL",
            "nul.dll",
            "COM1.exe",
            "lpt9",
            "CONOUT$",
            "manifest.json",
            "MANIFEST.JSON",
            "é.dll",
        ] {
            let mut manifest = valid();
            manifest["extra_files"][0]["file"] = json!(bad);
            assert_eq!(
                parse_value(&manifest),
                Err(ManifestError::BadFileName(bad.into())),
                "{bad:?}"
            );
        }
        for good in ["libcronet.dll", "COM10.dll", "console.dll", "a-b_c.1.dll"] {
            assert!(is_plain_file_name(good), "{good}");
        }
    }

    #[test]
    fn a_file_named_twice_is_refused_whatever_its_case() {
        let mut manifest = valid();
        manifest["extra_files"][0]["file"] = json!("Sing-Box.EXE");
        assert_eq!(
            parse_value(&manifest),
            Err(ManifestError::DuplicateFile("Sing-Box.EXE".into()))
        );
    }

    #[test]
    fn hashes_are_64_lowercase_hex_digits() {
        for bad in [
            &HASH[..63],
            &HASH.to_uppercase(),
            &format!("{}0", HASH),
            &HASH.replace('e', "g"),
        ] {
            let mut manifest = valid();
            manifest["sing_box"]["sha256"] = json!(bad);
            assert_eq!(
                parse_value(&manifest),
                Err(ManifestError::BadHash {
                    file: "sing-box.exe".into()
                })
            );
        }
    }

    #[test]
    fn files_are_verified_by_their_bytes() {
        assert_eq!(sha256_hex(&b""[..]).unwrap(), HASH);
        assert_eq!(sha256_hex(&b"hello"[..]).unwrap(), HASH2);
        assert!(verify("sing-box.exe", HASH2, &b"hello"[..]).is_ok());
        assert!(matches!(
            verify("sing-box.exe", HASH2, &b"hellO"[..]),
            Err(VerifyError::Mismatch { file }) if file == "sing-box.exe"
        ));
    }
}
