//! TUN through the privileged helper (ADR 0006): the GUI's side, minus the
//! Windows I/O.
//!
//! On Windows, TUN needs privileges the logged-in user doesn't have. Rather
//! than elevating itself, BoxPilot asks `boxpilot-helper`, a demand-start
//! service the MSI installs, to run *its own* sing-box on a config *it has
//! checked*. The privilege at stake is BoxPilot's to lend, so the profile
//! runs under the config policy there; privilege the user brings of their
//! own accord (BoxPilot run as Administrator) runs the profile as written,
//! with no helper ("Whose task is it").
//!
//! Everything that decides something lives here and is tested on every
//! platform:
//!
//! - [`start_route`]: which sing-box a start runs;
//! - [`prepare_start`]: the canonical config → its local files, read *as the
//!   user* through an injected reader → attachments → the policy, run here
//!   first so a refused profile never reaches the helper and the user learns
//!   which field and why → the `start` request;
//! - [`ConnectPlan`]: what to do when the helper's pipe isn't there (start
//!   its service, back off, read its exit code);
//! - the user's words for every refusal, error and exit code the helper or
//!   the policy can give, with a fallback for codes this build doesn't know;
//! - [`client`]: the connection itself, over any byte stream.
//!
//! The Windows module is only the pipe, the service calls and the token
//! check.

pub mod client;
#[cfg(target_os = "windows")]
mod windows;

use crate::core::paths::runtime_config_path;
use crate::core::settings::AppSettings;
use crate::core::singbox_api::SingBoxApi;
use crate::core::subscription::save_runtime_config;
use crate::i18n::s;
use boxpilot_policy::{attach, local_file_fields};
use boxpilot_protocol::endpoint::exit;
use boxpilot_protocol::{ErrorCode, RefusalCode, StartRequest, TunOptions, WireRefusal};
use boxpilot_runconfig::{inject, ApiService, Inject};
use futures_channel::mpsc::UnboundedReceiver;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use client::{start_session, HelperConnection, HelperEvent, HelperFailure, HelperIo};

/// Whether TUN on this platform goes through the privileged helper when
/// BoxPilot runs without privilege of its own. Windows only, for now:
/// Linux keeps its setcap copy (ADR 0003), and macOS is the helper's second
/// phase.
pub const HELPER_PLATFORM: bool = cfg!(target_os = "windows");

/// Which sing-box a start runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartRoute {
    /// BoxPilot's own child, at the privilege BoxPilot runs with, on the
    /// profile as written (ADR 0002): Proxy mode everywhere, TUN on Linux
    /// (ADR 0003's gate decides the binary), and TUN on Windows when the
    /// user runs BoxPilot as Administrator of their own accord.
    Local,
    /// The privileged helper's sing-box, on the config its policy checked:
    /// TUN on Windows from a BoxPilot that isn't elevated.
    Helper,
}

/// The route for a start. `helper_platform`: [`HELPER_PLATFORM`];
/// `elevated`: BoxPilot itself runs elevated, which only the user can have
/// arranged, since BoxPilot never asks for it.
pub fn start_route(helper_platform: bool, proxy_mode: bool, elevated: bool) -> StartRoute {
    if helper_platform && !proxy_mode && !elevated {
        StartRoute::Helper
    } else {
        StartRoute::Local
    }
}

/// Whether BoxPilot runs elevated (Windows: an elevated token). Only the
/// user can make it so; BoxPilot never asks. Always `false` elsewhere: Linux
/// decides by its own probe (`core::privilege`), and macOS has no TUN yet.
pub fn process_is_elevated() -> bool {
    #[cfg(target_os = "windows")]
    {
        windows::process_is_elevated()
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// Connect to the helper, starting its service if need be.
pub fn open() -> Result<Arc<dyn HelperIo>, OpenError> {
    #[cfg(target_os = "windows")]
    {
        windows::open()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err(OpenError::Unsupported)
    }
}

/// The typed options of a helper start, from the settings. The helper
/// never lets its sing-box write the system proxy (a SYSTEM sing-box would
/// write SYSTEM's, not the user's), so `system_proxy` only says what the
/// user asked for; the GUI sets the user's proxy itself
/// (`process::enable_system_proxy`).
pub fn tun_options(settings: &AppSettings) -> TunOptions {
    TunOptions {
        ipv6: settings.tun_ipv6,
        proxy_port: settings.proxy_port,
        allow_lan: settings.allow_lan,
        system_proxy: settings.set_system_proxy,
    }
}

// ---- A whole start ----

/// A helper start that runs: sing-box lives as long as `connection`.
pub struct RunningStart {
    pub connection: HelperConnection,
    /// Its sing-box API, on the port and secret the helper picked.
    pub api: SingBoxApi,
    /// Its lines and exit (`HelperEvent`).
    pub events: UnboundedReceiver<HelperEvent>,
}

/// Start the active profile through the helper: read its canonical config,
/// [`prepare_start`] it (its local files read as the user, against
/// `app_dir` as sing-box would resolve them; the policy run here first),
/// write the running view, then connect and start. Blocking: run it off
/// the UI thread. `Err` is the message for the user.
pub fn start_profile(
    config_path: &Path,
    app_dir: &Path,
    options: TunOptions,
) -> Result<RunningStart, String> {
    let canonical = fs::read_to_string(config_path).map_err(|e| {
        (s().errors.read_failed)(&config_path.display().to_string(), &e.to_string())
    })?;
    let prepared = prepare_start(&canonical, options, |path, limit| {
        read_as_user(app_dir, path, limit)
    })
    .map_err(|e| e.message())?;
    // Only for BoxPilot's own eyes (Settings › Troubleshooting, the VPN
    // endpoints it lists): a failure here doesn't stop TUN.
    let runtime = runtime_config_path(app_dir);
    if let Err(e) = save_runtime_config(&runtime, &prepared.running_view) {
        eprintln!("Failed to write {}: {e}", runtime.display());
    }
    let io = open().map_err(|e| e.message())?;
    let (connection, api, events) =
        start_session(io, prepared.request).map_err(|failure| failure.message())?;
    Ok(RunningStart {
        connection,
        api,
        events,
    })
}

/// Read a file a profile names, as the user (BoxPilot's own access decides
/// what it can read), where sing-box would look for it: a relative path
/// against its working directory, BoxPilot's data dir. At most `limit + 1`
/// bytes, so an oversized file is caught without reading all of it.
fn read_as_user(app_dir: &Path, path: &str, limit: usize) -> io::Result<Vec<u8>> {
    let file = File::open(app_dir.join(path))?;
    let mut data = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut data)?;
    Ok(data)
}

// ---- Preparing a start ----

/// A start ready to send.
pub struct PreparedStart {
    pub request: StartRequest,
    /// What BoxPilot shows and reads as the running config while the helper
    /// runs this start (`running_config.json`: Settings › Troubleshooting,
    /// and the VPN endpoints it lists). See [`running_view`].
    pub running_view: String,
}

/// Why a start never reached the helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareError {
    /// The policy refused the config, as the helper would.
    Refused(Vec<WireRefusal>),
    /// A local file the config reads couldn't be read as the user.
    ReadFile {
        path: String,
        pointer: String,
        error: String,
    },
    /// More distinct local files than one `start` may carry.
    TooManyFiles { count: usize, limit: usize },
    /// The config and its files are more than one `start` may carry.
    TooLarge { bytes: usize, limit: usize },
}

impl PrepareError {
    pub fn message(&self) -> String {
        let h = &s().helper;
        match self {
            PrepareError::Refused(refusals) => refused_message(refusals, 0),
            PrepareError::ReadFile {
                path,
                pointer,
                error,
            } => (h.read_file)(path, pointer, error),
            PrepareError::TooManyFiles { count, limit } => {
                (h.too_many_files)(*count as u64, *limit as u64)
            }
            PrepareError::TooLarge { bytes, limit } => {
                (h.start_too_large)(*bytes as u64, *limit as u64)
            }
        }
    }
}

/// The id of the `n`th distinct local file (1-based): within the
/// attachment-id charset (`boxpilot_policy::is_attachment_id`), and never a
/// path, which stays on this side.
fn attachment_id(n: usize) -> String {
    format!("file-{n}")
}

/// Turn the active profile's canonical config into a helper `start`:
///
/// 1. every local file the config reads (`local_file_fields`) is read by
///    `read_file` — as the user, the user's own access deciding what a
///    profile may read — and sent as an attachment; a path named twice is
///    sent once. `read_file(path, limit)` gets the path as the config
///    writes it (the caller resolves a relative one the way sing-box would,
///    against its working directory) and may stop after `limit + 1` bytes:
///    more than `limit` is too large;
/// 2. each field is pointed at its attachment (`attach`);
/// 3. the policy checks the result here first, so a refused profile never
///    reaches the helper; the helper checks again, and only its verdict
///    counts.
pub fn prepare_start(
    canonical: &str,
    options: TunOptions,
    mut read_file: impl FnMut(&str, usize) -> io::Result<Vec<u8>>,
) -> Result<PreparedStart, PrepareError> {
    let limits = boxpilot_protocol::Limits::default();
    let policy_limits = boxpilot_policy::Limits::default();
    let refused = |refusals: Vec<boxpilot_policy::Refusal>| {
        PrepareError::Refused(refusals.iter().map(WireRefusal::from).collect())
    };
    // Too large, too deep or not JSON: the policy says so before anything
    // is read, in the helper's own words.
    let unparsable = || match boxpilot_policy::check(canonical, &Default::default(), &policy_limits)
    {
        Err(refusals) => refused(refusals),
        // Unreachable: what serde_json can't parse, the policy refuses too.
        Ok(_) => PrepareError::Refused(vec![WireRefusal {
            pointer: String::new(),
            code: RefusalCode::InvalidJson,
            detail: None,
        }]),
    };
    if canonical.len() > policy_limits.max_bytes {
        return Err(unparsable());
    }
    let Ok(mut config) = serde_json::from_str::<Value>(canonical) else {
        return Err(unparsable());
    };

    let fields = local_file_fields(&config);
    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    let mut attachments: Vec<(String, Vec<u8>)> = Vec::new();
    // The config travels too; its length after `attach` differs from this
    // by a few bytes per field, and the exact sum is checked below.
    let mut total = canonical.len();
    for field in &fields {
        if let Some(id) = ids.get(&field.path) {
            attach(&mut config, &field.pointer, id).expect("a listed field with a valid id");
            continue;
        }
        if attachments.len() == limits.max_attachments {
            let distinct = fields
                .iter()
                .map(|f| f.path.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            return Err(PrepareError::TooManyFiles {
                count: distinct,
                limit: limits.max_attachments,
            });
        }
        let room = limits.max_start_total.saturating_sub(total);
        let data = read_file(&field.path, room).map_err(|error| PrepareError::ReadFile {
            path: field.path.clone(),
            pointer: field.pointer.clone(),
            error: error.to_string(),
        })?;
        if data.len() > room {
            return Err(PrepareError::TooLarge {
                bytes: total + data.len(),
                limit: limits.max_start_total,
            });
        }
        total += data.len();
        let id = attachment_id(attachments.len() + 1);
        attach(&mut config, &field.pointer, &id).expect("a listed field with a valid id");
        ids.insert(field.path.clone(), id.clone());
        attachments.push((id, data));
    }

    let text = serde_json::to_string(&config).expect("a parsed config serializes");
    let bytes = text.len()
        + attachments
            .iter()
            .map(|(_, data)| data.len())
            .sum::<usize>();
    if bytes > limits.max_start_total {
        return Err(PrepareError::TooLarge {
            bytes,
            limit: limits.max_start_total,
        });
    }

    let request = StartRequest {
        config: text,
        attachments,
        options,
    };
    let checked =
        boxpilot_policy::check(&request.config, &request.attachment_ids(), &policy_limits)
            .map_err(refused)?;
    Ok(PreparedStart {
        running_view: running_view(checked.config(), &options),
        request,
    })
}

/// The running config as BoxPilot knows it for a helper start: the config
/// the policy passed (control planes and helper-owned fields dropped,
/// local files as attachment references) with BoxPilot's inbounds for
/// `options` injected. Without the helper's own `api` service: its secret
/// never touches disk on this side. What the helper runs differs only in
/// where it put the attachments and the cache file, and that service.
pub fn running_view(checked: &Value, options: &TunOptions) -> String {
    let mut config = checked.clone();
    if let Some(root) = config.as_object_mut() {
        let placeholder = ApiService::new(0, &[]);
        inject(
            root,
            &Inject {
                proxy_mode: false,
                set_system_proxy: options.system_proxy,
                forbid_system_proxy: true,
                proxy_port: options.proxy_port,
                tun_ipv6: options.ipv6,
                allow_lan: options.allow_lan,
                api: &placeholder,
            },
        );
        // The checked config has no `services` of its own (the policy drops
        // them), so the only one is the placeholder.
        root.remove("services");
    }
    serde_json::to_string_pretty(&config).expect("a JSON value serializes")
}

// ---- Reaching the helper ----

/// Why the helper couldn't be reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// No `BoxPilotHelper` service: a portable copy without the MSI.
    NotInstalled,
    /// The service is disabled.
    Disabled,
    /// Windows refused to start the service for this account.
    StartDenied,
    /// Windows refused this account the helper's pipe.
    ConnectDenied,
    /// The service stopped with one of the helper's exit codes
    /// (`endpoint::exit`).
    ServiceExited(i32),
    /// The service failed with a Windows error code.
    ServiceFailed(u32),
    /// The pipe didn't appear, or stayed busy, for too long.
    TimedOut,
    /// Anything else the OS said.
    Os(String),
    /// No helper on this platform.
    Unsupported,
}

impl OpenError {
    pub fn message(&self) -> String {
        let h = &s().helper;
        match self {
            OpenError::NotInstalled => h.not_installed.to_string(),
            OpenError::Disabled => h.disabled.to_string(),
            OpenError::StartDenied => h.start_denied.to_string(),
            OpenError::ConnectDenied => h.connect_denied.to_string(),
            OpenError::ServiceExited(code) => exit_code_message(*code),
            OpenError::ServiceFailed(code) => (h.service_failed)(&code.to_string()),
            OpenError::TimedOut => h.timed_out.to_string(),
            OpenError::Os(error) => (h.unreachable)(error),
            OpenError::Unsupported => h.unsupported.to_string(),
        }
    }
}

/// Win32 error codes the connect plan tells apart. Plain numbers, so the
/// plan is tested off Windows too.
pub mod win32 {
    pub const ERROR_FILE_NOT_FOUND: u32 = 2;
    pub const ERROR_ACCESS_DENIED: u32 = 5;
    pub const ERROR_PIPE_BUSY: u32 = 231;
    pub const ERROR_SERVICE_ALREADY_RUNNING: u32 = 1056;
    pub const ERROR_SERVICE_DISABLED: u32 = 1058;
    pub const ERROR_SERVICE_DOES_NOT_EXIST: u32 = 1060;
    pub const ERROR_SERVICE_SPECIFIC_ERROR: u32 = 1066;
    pub const ERROR_SERVICE_MARKED_FOR_DELETE: u32 = 1072;
}

/// What opening the helper's pipe said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeOpen {
    /// No instance listens: the service isn't running (or is between its
    /// idle exit and the next start).
    NotFound,
    /// Every instance is busy.
    Busy,
    Denied,
    Other(u32),
}

impl PipeOpen {
    pub fn from_win32(code: u32) -> Self {
        match code {
            win32::ERROR_FILE_NOT_FOUND => PipeOpen::NotFound,
            win32::ERROR_PIPE_BUSY => PipeOpen::Busy,
            win32::ERROR_ACCESS_DENIED => PipeOpen::Denied,
            other => PipeOpen::Other(other),
        }
    }
}

/// What asking the service to start said: `OpenSCManagerW`, `OpenServiceW`
/// and `StartServiceW` together (`None`: started).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStart {
    Started,
    AlreadyRunning,
    NotInstalled,
    Disabled,
    Denied,
    Failed(u32),
}

impl ServiceStart {
    pub fn from_win32(error: Option<u32>) -> Self {
        match error {
            None => ServiceStart::Started,
            Some(win32::ERROR_SERVICE_ALREADY_RUNNING) => ServiceStart::AlreadyRunning,
            // Marked for delete: an uninstall is under way.
            Some(win32::ERROR_SERVICE_DOES_NOT_EXIST | win32::ERROR_SERVICE_MARKED_FOR_DELETE) => {
                ServiceStart::NotInstalled
            }
            Some(win32::ERROR_SERVICE_DISABLED) => ServiceStart::Disabled,
            Some(win32::ERROR_ACCESS_DENIED) => ServiceStart::Denied,
            Some(other) => ServiceStart::Failed(other),
        }
    }
}

/// The service's status (`QueryServiceStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// Starting, running, or stopping.
    Active,
    Stopped {
        win32_exit: u32,
        service_exit: u32,
    },
}

/// How long the helper may take to appear: a demand start verifies its
/// folders and sing-box's hash before it listens.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const FIRST_BACKOFF: Duration = Duration::from_millis(50);
const MAX_BACKOFF: Duration = Duration::from_millis(500);
/// The longest single `WaitNamedPipeW` while every instance is busy.
const MAX_BUSY_WAIT: Duration = Duration::from_secs(2);

/// What to do while the helper's pipe can't be opened. The helper exits
/// after a minute idle, so a connect can race its exit: the pipe vanishes,
/// and the service is started again. Once this connect has started the
/// service, a missing pipe means "not listening yet" or "it failed", which
/// its status tells apart, so it is read before the service is started
/// again (a second start would overwrite the first one's exit code). Every
/// step has a deadline.
#[derive(Debug)]
pub struct ConnectPlan {
    deadline: Instant,
    backoff: Duration,
    /// This connect started the service, so a stop seen after it is this
    /// start's, not an earlier one's.
    started: bool,
}

/// The next step of a [`ConnectPlan`], before it backs off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectStep {
    /// Ask the service to start, and report with
    /// [`ConnectPlan::service_started`].
    StartService,
    /// Read the status of the service this connect started, and report
    /// with [`ConnectPlan::service_state`].
    CheckService,
    /// `WaitNamedPipeW` for at most this long, then open again.
    WaitBusy(Duration),
}

impl ConnectPlan {
    pub fn new(now: Instant) -> Self {
        Self {
            deadline: now + CONNECT_TIMEOUT,
            backoff: FIRST_BACKOFF,
            started: false,
        }
    }

    /// Opening the pipe failed with `error`.
    pub fn pipe_failed(&mut self, error: PipeOpen, now: Instant) -> Result<ConnectStep, OpenError> {
        if now >= self.deadline {
            return Err(OpenError::TimedOut);
        }
        match error {
            PipeOpen::NotFound if self.started => Ok(ConnectStep::CheckService),
            PipeOpen::NotFound => Ok(ConnectStep::StartService),
            PipeOpen::Busy => Ok(ConnectStep::WaitBusy(
                (self.deadline - now).min(MAX_BUSY_WAIT),
            )),
            PipeOpen::Denied => Err(OpenError::ConnectDenied),
            PipeOpen::Other(code) => Err(OpenError::Os(
                io::Error::from_raw_os_error(code as i32).to_string(),
            )),
        }
    }

    /// What asking the service to start said. Already running (it may be
    /// stopping after its idle exit) is waited out like a start.
    pub fn service_started(&mut self, start: ServiceStart) -> Result<(), OpenError> {
        match start {
            ServiceStart::Started => {
                self.started = true;
                Ok(())
            }
            ServiceStart::AlreadyRunning => Ok(()),
            ServiceStart::NotInstalled => Err(OpenError::NotInstalled),
            ServiceStart::Disabled => Err(OpenError::Disabled),
            ServiceStart::Denied => Err(OpenError::StartDenied),
            ServiceStart::Failed(code) => Err(OpenError::ServiceFailed(code)),
        }
    }

    /// The status of the service this connect started. `Ok(true)`: it
    /// stopped cleanly (an idle exit racing the start), so start it again;
    /// `Ok(false)`: still starting, so wait. A stop with an error is this
    /// start's failure: its own exit code, or a Windows one.
    pub fn service_state(&mut self, state: ServiceState) -> Result<bool, OpenError> {
        match state {
            ServiceState::Active => Ok(false),
            ServiceState::Stopped { win32_exit: 0, .. } => {
                self.started = false;
                Ok(true)
            }
            ServiceState::Stopped {
                win32_exit: win32::ERROR_SERVICE_SPECIFIC_ERROR,
                service_exit,
            } => Err(OpenError::ServiceExited(service_exit as i32)),
            ServiceState::Stopped { win32_exit, .. } => Err(OpenError::ServiceFailed(win32_exit)),
        }
    }

    /// How long to wait before opening the pipe again: doubling from 50 ms
    /// to 500 ms, never past the deadline.
    pub fn backoff(&mut self, now: Instant) -> Result<Duration, OpenError> {
        if now >= self.deadline {
            return Err(OpenError::TimedOut);
        }
        let wait = self.backoff.min(self.deadline - now);
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
        Ok(wait)
    }
}

// ---- What the user reads ----

/// How many refusals a message lists before it counts the rest.
const SHOWN_REFUSALS: usize = 3;

/// The words for what a `malformed` place takes, from its wire code.
fn expected_text(detail: Option<&str>) -> String {
    let h = &s().helper;
    match detail {
        Some("object") => h.expected_object.to_string(),
        Some("array") => h.expected_array.to_string(),
        Some("string") => h.expected_string.to_string(),
        Some("string_or_array") => h.expected_string_or_array.to_string(),
        Some("plugin_options") => h.expected_plugin_options.to_string(),
        Some(other) => other.to_string(),
        None => s().common.unknown.to_string(),
    }
}

/// One refusal in the user's words: where (the JSON pointer, or the whole
/// config) and why.
pub fn refusal_text(refusal: &WireRefusal) -> String {
    let h = &s().helper;
    let detail = refusal.detail.as_deref();
    let detail_or_unknown = || detail.unwrap_or(s().common.unknown);
    let reason = match &refusal.code {
        RefusalCode::TooLarge => (h.too_big)(detail_or_unknown()),
        RefusalCode::TooDeep => (h.too_deep)(detail_or_unknown()),
        RefusalCode::InvalidJson => (h.invalid_json)(detail_or_unknown()),
        RefusalCode::NotAnObject => h.not_an_object.to_string(),
        RefusalCode::Malformed => (h.malformed)(&expected_text(detail)),
        RefusalCode::NonCanonicalKey => h.non_canonical_key.to_string(),
        RefusalCode::UnknownSection => h.unknown_section.to_string(),
        RefusalCode::TypeNotAllowed => match detail {
            Some(type_name) => (h.type_not_allowed)(type_name),
            None => h.type_missing.to_string(),
        },
        RefusalCode::Inbounds => h.inbounds.to_string(),
        RefusalCode::Service => match detail {
            Some(service_type) => (h.service)(service_type),
            None => h.service_untyped.to_string(),
        },
        RefusalCode::UnknownExperimental => h.unknown_experimental.to_string(),
        RefusalCode::RunsProgram => h.runs_program.to_string(),
        RefusalCode::SystemChange => h.system_change.to_string(),
        RefusalCode::ServerFileScan => h.server_file_scan.to_string(),
        RefusalCode::FilesystemPath => h.filesystem_path.to_string(),
        RefusalCode::Directory => h.directory.to_string(),
        RefusalCode::LocalFile => h.local_file.to_string(),
        RefusalCode::MalformedAttachment => h.malformed_attachment.to_string(),
        RefusalCode::MissingAttachment => (h.missing_attachment)(detail_or_unknown()),
        RefusalCode::Other(code) => (h.unknown_refusal)(code),
    };
    let at = if refusal.pointer.is_empty() {
        h.whole_config
    } else {
        refusal.pointer.as_str()
    };
    (h.refusal_at)(at, &reason)
}

/// The message for a refused profile: the first few refusals, the rest
/// counted (`omitted` more were cut by the helper itself).
pub fn refused_message(refusals: &[WireRefusal], omitted: u32) -> String {
    let h = &s().helper;
    let mut parts: Vec<String> = refusals
        .iter()
        .take(SHOWN_REFUSALS)
        .map(refusal_text)
        .collect();
    let more = refusals.len().saturating_sub(SHOWN_REFUSALS) as u64 + u64::from(omitted);
    if more > 0 {
        parts.push((h.refused_more)(more));
    }
    (h.refused)(&parts.join(h.refusal_sep))
}

/// The message for an `error` reply. `ErrorCode` is closed: a code a newer
/// helper adds fails to decode, and reads as a reply BoxPilot doesn't
/// understand (`HelperFailure::BadReply`), which is the fallback.
pub fn error_message(code: ErrorCode, message: &str) -> String {
    let h = &s().helper;
    match code {
        ErrorCode::Unauthorized => h.not_allowed.to_string(),
        ErrorCode::VersionMismatch => h.version_mismatch.to_string(),
        ErrorCode::Busy => h.busy.to_string(),
        ErrorCode::BadRequest => (h.bad_request)(message),
        ErrorCode::Internal => (h.internal)(message),
    }
}

/// The message for a helper that stopped with `code`, its service-specific
/// exit code.
pub fn exit_code_message(code: i32) -> String {
    let h = &s().helper;
    let reason = match code {
        exit::USAGE => h.exit_usage.to_string(),
        exit::UNSUPPORTED_OS => h.exit_unsupported_os.to_string(),
        exit::HELPER_DIR_REFUSED => h.exit_helper_dir.to_string(),
        exit::STATE_DIR_REFUSED => h.exit_state_dir.to_string(),
        exit::MANIFEST_REFUSED => h.exit_manifest.to_string(),
        exit::PIPE_SQUATTED => h.exit_pipe_squatted.to_string(),
        exit::PIPE_FAILED => h.exit_pipe_failed.to_string(),
        exit::CONSOLE_ELEVATED => h.exit_console_elevated.to_string(),
        exit::PRIVILEGES_REFUSED => h.exit_privileges.to_string(),
        exit::INTERNAL => h.exit_internal.to_string(),
        other => (h.exit_unknown)(&other.to_string()),
    };
    (h.exited)(&reason)
}

#[cfg(test)]
mod tests;
