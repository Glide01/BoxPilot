//! The config policy of BoxPilot's privileged helper (ADR 0006, rule 2).
//!
//! In helper TUN mode sing-box runs as root or SYSTEM, on a profile's config,
//! which is remote input. This crate decides what such a sing-box may be
//! started with. It is pure (no I/O, no gpui), so the GUI runs it to explain
//! a refusal before it asks, and the helper runs it again on what arrives;
//! only the helper's verdict counts.
//!
//! - [`check`] turns a profile's canonical config into a [`Checked`] config,
//!   or into every [`Refusal`] it earns.
//! - [`materialize`] points the fields the helper owns at its own files.
//! - [`local_file_fields`] and [`attach`] let the GUI turn the files a
//!   profile reads into attachments, which it reads as the user.
//!
//! The rule is deny by shape: a key that looks like a filesystem location is
//! refused unless the helper fills it, it is a file read that travels as an
//! attachment, or it is on a context-scoped list of keys that name no file
//! (URL paths, process-path matchers). A path field upstream adds next year
//! fails closed. Field names were checked against sing-box 1.14.2 (`option/`
//! and the docs); audit them at every `SINGBOX_VERSION` bump.
//!
//! Rules name keys exactly, in lower case. sing-box's Go decoder matches
//! field names case-insensitively (`Executable_Path` is `executable_path` to
//! it), so a key spelled any other way is refused rather than guessed at
//! ([`RefusalKind::NonCanonicalKey`]).
//!
//! The helper must run sing-box on the serialization of [`materialize`]'s
//! output, never on the bytes it received: those can say things (duplicate
//! keys, JSON comments) that sing-box's Go decoder reads differently from
//! the parse this policy checked.

#![forbid(unsafe_code)]

mod walk;

use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;

/// What a file-read field holds once its file travels as an attachment:
/// this prefix, then the attachment's id.
pub const ATTACHMENT_PREFIX: &str = "boxpilot-attachment:";

/// The longest attachment id.
pub const MAX_ATTACHMENT_ID_LEN: usize = 64;

/// Whether `id` can name an attachment: 1 to 64 of `A-Z a-z 0-9 _ -`. Ids
/// are the caller's, so they never become file names as they are (see
/// [`Placement::attachment_path`]); the charset only keeps references
/// unambiguous and short.
pub fn is_attachment_id(id: &str) -> bool {
    (1..=MAX_ATTACHMENT_ID_LEN).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Hard limits on the config text, applied before anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Larger text is refused before it is parsed.
    pub max_bytes: usize,
    /// Deeper nesting is refused before it is parsed. serde_json stops at
    /// 128 levels on its own, so a larger value has no further effect.
    pub max_depth: usize,
}

impl Default for Limits {
    /// 32 MiB, as the helper's frame cap (ADR 0006 rule 1): room for real
    /// profiles with inline rule sets. 64 levels: real configs nest about
    /// ten deep, logical rules included.
    fn default() -> Self {
        Self {
            max_bytes: 32 * 1024 * 1024,
            max_depth: 64,
        }
    }
}

/// One reason the config can't run on the privileged path, at one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// JSON Pointer (RFC 6901) to the offending key; empty for the whole
    /// config.
    pub pointer: String,
    pub kind: RefusalKind,
}

/// Why something is refused. Specific enough for the GUI to say it in the
/// user's language; [`fmt::Display`] is plain English for logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusalKind {
    /// The text is larger than [`Limits::max_bytes`]; it was not parsed.
    TooLarge { bytes: usize, limit: usize },
    /// The text nests deeper than [`Limits::max_depth`]; it was not parsed.
    TooDeep { limit: usize },
    /// The text is not JSON. serde_json's message, with line and column.
    InvalidJson(String),
    /// The config is not a JSON object.
    NotAnObject,
    /// A value of a type sing-box doesn't take there, where the policy has to
    /// look inside it (say `outbounds` that isn't an array).
    Malformed { expected: Expected },
    /// A key spelled with upper-case letters (or `ſ`, or the Kelvin sign).
    /// sing-box matches field names case-insensitively, so it would read
    /// `Executable_Path` as `executable_path`; the policy refuses rather than
    /// guess which field it is.
    NonCanonicalKey,
    /// A top-level key outside the sections the helper runs.
    UnknownSection,
    /// `inbounds`: BoxPilot owns the inbounds, and the helper adds its own.
    Inbounds,
    /// A `services` entry other than `api` (USB/IP, DERP, ssm-api, ccm/ocm,
    /// resolved, …), with its `type` when it has one.
    Service { service_type: Option<String> },
    /// An `experimental` key other than `cache_file`, `clash_api` and
    /// `v2ray_api`.
    UnknownExperimental,
    /// Runs a program: the tor outbound, an `executable_path`, OpenConnect's
    /// `*wrapper_path`.
    RunsProgram,
    /// Changes the system beyond networking: NTP `write_to_system`, the
    /// Tailscale SSH server.
    SystemChange,
    /// Lets the remote server inspect local files: an OpenConnect endpoint
    /// in the AnyConnect flavor runs a built-in host scan that stats the
    /// files the server names and reports their CRC32.
    ServerFileScan,
    /// Names a filesystem location that is neither filled by the helper nor
    /// an attachment (deny by shape).
    FilesystemPath,
    /// A directory sing-box reads; a directory can't travel as an
    /// attachment.
    Directory,
    /// A file sing-box reads, given as a local path: the GUI attaches it
    /// ([`local_file_fields`], [`attach`]).
    LocalFile,
    /// An attachment reference whose id is malformed.
    MalformedAttachment,
    /// An attachment reference to an id the request doesn't carry.
    MissingAttachment { id: String },
}

/// The kind of value a [`RefusalKind::Malformed`] place takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expected {
    Object,
    Array,
    String,
    /// A string, or an array of strings.
    StringOrArray,
    /// Shadowsocks SIP003 `plugin_opts` that parse (`k=v;k=v`, `\` escapes).
    PluginOptions,
}

/// Something `check` removed: it does not run on the privileged path, or
/// the helper fills it itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropped {
    /// JSON Pointer (RFC 6901) to the removed key, in the input.
    pub pointer: String,
    pub reason: DropReason,
}

/// Why a key was dropped rather than refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// A control plane over the privileged sing-box: `clash_api`,
    /// `v2ray_api`, an `api` service. Only the helper's own `api` runs there.
    ControlPlane,
    /// A field the helper fills: `log.output`, `cache_file.path`, each
    /// Tailscale endpoint's `state_directory` and `taildrop_directory`.
    HelperOwned,
    /// `$schema`, an editor annotation sing-box ignores.
    Annotation,
}

/// A config that passed [`check`]. Only `check` makes one, so the config
/// [`materialize`] rewrites is always a checked one.
#[derive(Debug, Clone)]
pub struct Checked {
    config: Value,
    dropped: Vec<Dropped>,
    refs: Vec<AttachmentRef>,
    tailscale: Vec<TailscaleEndpoint>,
}

/// An attachment reference `check` accepted, where it stands.
#[derive(Debug, Clone)]
struct AttachmentRef {
    pointer: String,
    id: String,
}

/// A Tailscale endpoint, by pointer, with the tag sing-box runs it under.
#[derive(Debug, Clone)]
struct TailscaleEndpoint {
    pointer: String,
    tag: String,
}

impl Checked {
    /// The config without what was dropped: no `services`, no `inbounds`,
    /// no control planes, no helper-owned fields. Attachment references are
    /// still references.
    pub fn config(&self) -> &Value {
        &self.config
    }

    /// What `check` removed, in walk order.
    pub fn dropped(&self) -> &[Dropped] {
        &self.dropped
    }

    /// The attachments the config refers to: the only ones the helper needs
    /// to write.
    pub fn attachment_ids(&self) -> BTreeSet<&str> {
        self.refs.iter().map(|r| r.id.as_str()).collect()
    }
}

/// Check a profile's canonical config (JSON text, as `strip_inbounds` stores
/// it) for the privileged path. `attachments` are the ids of the attachments
/// the request carries. Every refusal is collected, not just the first.
pub fn check(
    config: &str,
    attachments: &BTreeSet<String>,
    limits: &Limits,
) -> Result<Checked, Vec<Refusal>> {
    let whole = |kind| {
        vec![Refusal {
            pointer: String::new(),
            kind,
        }]
    };
    // Both limits hold before serde_json allocates anything.
    if config.len() > limits.max_bytes {
        return Err(whole(RefusalKind::TooLarge {
            bytes: config.len(),
            limit: limits.max_bytes,
        }));
    }
    if walk::nesting_exceeds(config, limits.max_depth) {
        return Err(whole(RefusalKind::TooDeep {
            limit: limits.max_depth,
        }));
    }
    let mut config: Value =
        serde_json::from_str(config).map_err(|e| whole(RefusalKind::InvalidJson(e.to_string())))?;
    let Value::Object(root) = &config else {
        return Err(whole(RefusalKind::NotAnObject));
    };
    let walked = walk::Walk::new(attachments).root(root);
    if !walked.refusals.is_empty() {
        return Err(walked.refusals);
    }
    // Removing keys moves no other value, so every pointer the walk
    // recorded still holds.
    for (parent, key) in &walked.removals {
        config
            .pointer_mut(parent)
            .and_then(Value::as_object_mut)
            .expect("the walk drops keys of objects it walked")
            .shift_remove(key);
    }
    Ok(Checked {
        config,
        dropped: walked.dropped,
        refs: walked.refs,
        tailscale: walked.tailscale,
    })
}

/// Where the helper puts what it owns, for one start. Strings rather than
/// paths: the crate is pure, and the helper's OS decides the separator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// The fresh, root-only run directory the attachments are written into.
    pub run_dir: String,
    /// sing-box's cache file (`experimental.cache_file.path`), in the
    /// helper's tree.
    pub cache_file: String,
    /// The owner's Tailscale state, in the helper's tree. Each endpoint gets
    /// a directory in it, by tag, so a login survives reconnects.
    pub tailscale_dir: String,
    /// The helper's path separator, `std::path::MAIN_SEPARATOR`. A field
    /// rather than the constant, so the result reads the same on every OS.
    pub separator: char,
}

impl Placement {
    /// Where the helper writes attachment `id`, and where the config then
    /// points: in the run directory, under a name the helper derives. The id
    /// is hex-encoded behind a fixed prefix, so no id can become a separator,
    /// a `..`, a Windows device name (`CON`, `NUL`), or collide with another
    /// on a case-insensitive file system.
    pub fn attachment_path(&self, id: &str) -> String {
        self.join(&self.run_dir, &[&format!("attachment-{}", hex(id))])
    }

    /// The Tailscale `state_directory` for the endpoint tagged `tag`.
    pub fn tailscale_state_dir(&self, tag: &str) -> String {
        self.join(&self.tailscale_dir, &[&endpoint_dir(tag), "state"])
    }

    /// The Tailscale `taildrop_directory` for the endpoint tagged `tag`.
    pub fn tailscale_taildrop_dir(&self, tag: &str) -> String {
        self.join(&self.tailscale_dir, &[&endpoint_dir(tag), "taildrop"])
    }

    fn join(&self, base: &str, parts: &[&str]) -> String {
        let mut path = base.trim_end_matches(self.separator).to_owned();
        for part in parts {
            path.push(self.separator);
            path.push_str(part);
        }
        path
    }
}

/// One endpoint's directory under `Placement::tailscale_dir`: its tag is
/// profile content, so it is hex-encoded like an attachment id.
fn endpoint_dir(tag: &str) -> String {
    format!("endpoint-{}", hex(tag))
}

fn hex(text: &str) -> String {
    text.bytes().map(|b| format!("{b:02x}")).collect()
}

/// The config the helper runs, short of the inbounds and the `api` service it
/// injects itself: every attachment reference becomes the file the helper
/// wrote, `experimental.cache_file.path` is the helper's cache file, and
/// each Tailscale endpoint keeps its state in the helper's tree.
pub fn materialize(checked: Checked, placement: &Placement) -> Value {
    let Checked {
        mut config,
        refs,
        tailscale,
        ..
    } = checked;
    for r in refs {
        let slot = config
            .pointer_mut(&r.pointer)
            .expect("check recorded the pointer of an existing reference");
        *slot = Value::String(placement.attachment_path(&r.id));
    }
    for endpoint in tailscale {
        let object = config
            .pointer_mut(&endpoint.pointer)
            .and_then(Value::as_object_mut)
            .expect("check recorded the pointer of an endpoint object");
        object.insert(
            "state_directory".into(),
            Value::String(placement.tailscale_state_dir(&endpoint.tag)),
        );
        object.insert(
            "taildrop_directory".into(),
            Value::String(placement.tailscale_taildrop_dir(&endpoint.tag)),
        );
    }
    // Set even when the profile has no `cache_file`: whatever enables the
    // cache later (the helper's injection does) finds the helper's path.
    let root = config.as_object_mut().expect("check took an object");
    let cache_file = object_entry(object_entry(root, "experimental"), "cache_file");
    cache_file.insert("path".into(), Value::String(placement.cache_file.clone()));
    config
}

/// The object at `parent[key]`, created if absent. `check` leaves no
/// non-object `experimental` or `cache_file`.
fn object_entry<'a>(parent: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    parent
        .entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .expect("check refused a non-object here")
}

/// A file-read field whose value is a local path, not yet an attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFileField {
    /// JSON Pointer (RFC 6901) to the value: the field, or one element of an
    /// array-valued field.
    pub pointer: String,
    pub path: String,
}

/// The files the config reads from local paths: the GUI reads each one as
/// the user, sends it as an attachment, and [`attach`]es its id here. An
/// empty path names no file and is not listed.
pub fn local_file_fields(config: &Value) -> Vec<LocalFileField> {
    let Value::Object(root) = config else {
        return Vec::new();
    };
    walk::Walk::new(&BTreeSet::new()).root(root).local_files
}

/// Why [`attach`] changed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachError {
    /// The id is not 1–64 of `A-Z a-z 0-9 _ -`.
    MalformedId,
    /// The pointer is not one [`local_file_fields`] lists.
    NotALocalFile,
}

impl fmt::Display for AttachError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttachError::MalformedId => f.write_str("not a valid attachment id"),
            AttachError::NotALocalFile => f.write_str("not a local file the config reads"),
        }
    }
}

impl std::error::Error for AttachError {}

/// Replace the local path at `pointer` (one of [`local_file_fields`]) with a
/// reference to attachment `id`. Anything else is left alone: a URL path or
/// a helper-owned field never becomes a reference by mistake.
pub fn attach(config: &mut Value, pointer: &str, id: &str) -> Result<(), AttachError> {
    if !is_attachment_id(id) {
        return Err(AttachError::MalformedId);
    }
    if !local_file_fields(config)
        .iter()
        .any(|field| field.pointer == pointer)
    {
        return Err(AttachError::NotALocalFile);
    }
    let slot = config
        .pointer_mut(pointer)
        .expect("local_file_fields lists existing values");
    *slot = Value::String(format!("{ATTACHMENT_PREFIX}{id}"));
    Ok(())
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.pointer.is_empty() {
            write!(f, "the config {}", self.kind)
        } else {
            write!(f, "{} {}", self.pointer, self.kind)
        }
    }
}

impl fmt::Display for RefusalKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RefusalKind::TooLarge { bytes, limit } => {
                write!(f, "is {bytes} bytes, over the {limit}-byte limit")
            }
            RefusalKind::TooDeep { limit } => write!(f, "nests deeper than {limit} levels"),
            RefusalKind::InvalidJson(message) => write!(f, "is not valid JSON: {message}"),
            RefusalKind::NotAnObject => f.write_str("is not a JSON object"),
            RefusalKind::Malformed { expected } => write!(f, "is not {expected}"),
            RefusalKind::NonCanonicalKey => f.write_str(
                "is not spelled in lower case; sing-box matches field names \
                 case-insensitively, so it can't be checked",
            ),
            RefusalKind::UnknownSection => {
                f.write_str("is not a section sing-box may run with on the privileged path")
            }
            RefusalKind::Inbounds => {
                f.write_str("defines inbounds; BoxPilot adds its own on the privileged path")
            }
            RefusalKind::Service {
                service_type: Some(service_type),
            } => write!(
                f,
                "runs a `{service_type}` service, which the privileged path doesn't allow"
            ),
            RefusalKind::Service { service_type: None } => {
                f.write_str("runs a service, which the privileged path doesn't allow")
            }
            RefusalKind::UnknownExperimental => {
                f.write_str("is an experimental option the privileged path doesn't allow")
            }
            RefusalKind::RunsProgram => f.write_str("runs a program"),
            RefusalKind::SystemChange => f.write_str("changes the system beyond networking"),
            RefusalKind::ServerFileScan => f.write_str(
                "lets the VPN server inspect local files (the built-in AnyConnect host scan)",
            ),
            RefusalKind::FilesystemPath => f.write_str("names a filesystem location"),
            RefusalKind::Directory => {
                f.write_str("names a directory to read, which can't travel as an attachment")
            }
            RefusalKind::LocalFile => f.write_str("reads a local file that isn't attached"),
            RefusalKind::MalformedAttachment => f.write_str("is not a valid attachment reference"),
            RefusalKind::MissingAttachment { id } => {
                write!(
                    f,
                    "refers to attachment `{id}`, which the request doesn't carry"
                )
            }
        }
    }
}

impl fmt::Display for Expected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Expected::Object => "an object",
            Expected::Array => "an array",
            Expected::String => "a string",
            Expected::StringOrArray => "a string or an array of strings",
            Expected::PluginOptions => "valid SIP003 plugin options",
        })
    }
}

impl fmt::Display for Dropped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} dropped: {}", self.pointer, self.reason)
    }
}

impl fmt::Display for DropReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DropReason::ControlPlane => "a control plane doesn't run on the privileged path",
            DropReason::HelperOwned => "the helper fills it",
            DropReason::Annotation => "an annotation sing-box ignores",
        })
    }
}
