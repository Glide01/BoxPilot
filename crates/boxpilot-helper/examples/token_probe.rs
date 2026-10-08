//! `token_probe`: which token sing-box needs for TUN on Windows, measured
//! rather than guessed (ADR 0006, "Defense in depth").
//!
//! CI runs it as SYSTEM (a one-shot scheduled task that
//! `packaging/windows/helper-smoke.ps1 -Step token-probe` registers), on
//! the runner the MSI was just installed on, before anything else there has
//! brought TUN up. It starts the installed sing-box
//! (`%ProgramFiles%\BoxPilot\Helper\sing-box.exe`) again and again, down
//! the helper's own spawn path (`boxpilot_helper::win::probe`: the
//! restricted token, the job object, the handle list, the mitigations, the
//! environment built from nothing), on the config the helper writes for a
//! minimal TUN profile (the smoke client's: a direct outbound, DNS
//! hijacked, `auto_route` and `strict_route`, a local rule set as an
//! attachment), each time under another token, and records what happened.
//!
//! In this order:
//!
//! 1. **First install.** The runner has never had wintun, so the first
//!    adapter sing-box creates installs wintun's driver. The smallest token
//!    goes first, then ever larger ones (see `rungs`), until the driver
//!    installs and the adapter comes up: `{SeChangeNotifyPrivilege}`, then
//!    `+SeLoadDriverPrivilege`, then every low-risk privilege, then the
//!    impersonation ones, then the file-access ones, then everything.
//! 2. **Steady state.** With the driver installed (and unloaded again
//!    once its adapter is gone, which the log shows, so each trial loads
//!    it), the smallest set with which TUN fully works:
//!    `{SeChangeNotifyPrivilege}` first, climbing the same rungs if it
//!    fails, then dropping one privilege at a time, the most dangerous
//!    first, keeping each drop that still works.
//! 3. **Narrowings** on that set: the integrity level lowered from System
//!    to High, `BUILTIN\Administrators` made deny-only, and both.
//! 4. **The shipped plan**, `tokenplan::SING_BOX_TOKEN`, as the helper
//!    starts sing-box: on a first install again (the driver package
//!    removed from the driver store first), then in steady state. This is
//!    the regression check CI holds every sing-box upgrade to: the step
//!    fails if the shipped plan stops working, or if TUN needed a privilege
//!    `NEVER_FOR_SING_BOX` names.
//! 5. **Install again** with the strictest token that worked in 2 and 3,
//!    when it is narrower than the shipped plan (data for tightening it).
//!    If it can't install, phase 1's privileges are added, with and
//!    without the narrowings, then dropped one at a time, each trial a
//!    first install again; the driver is always put back.
//!
//! Each trial: stale adapters removed and the DNS cache flushed; a fresh
//! run directory with the helper's config; sing-box started; its token read
//! back from its process; then whether it logged its TUN inbound and itself
//! started, whether this machine's own TCP connection to 1.1.1.1 leaves
//! from the TUN address, and whether a name resolves (through the hijacked
//! DNS); then sing-box stopped as the helper stops it (its job ended), and
//! whether its adapter went and the machine's own route came back; and the
//! wintun driver's state before and after. sing-box's own output is in the
//! log, so a failure says what sing-box said.
//!
//! It writes into `--work`: `probe.log` (everything), `summary.txt` (the
//! table and what it found), `result.txt` (`key=value` lines for the
//! script) and, last, `done` (`ok`, or `malfunction: <why>`). A trial that
//! fails is data, not a failure (the script judges the regression check
//! from `result.txt`): the exit code is 0 once every trial the
//! time budget allowed has run, 1 when the probe itself broke (not SYSTEM,
//! sing-box missing, a run directory it couldn't write, TUN down even with
//! every privilege), 2 for a bad command line. It is a test tool: the MSI
//! never installs it.

#[cfg(not(windows))]
fn main() {
    eprintln!("token_probe measures sing-box's token on Windows; it runs on Windows only");
    std::process::exit(boxpilot_protocol::endpoint::exit::UNSUPPORTED_OS);
}

#[cfg(windows)]
fn main() {
    std::process::exit(probe::main());
}

#[cfg(windows)]
mod probe {
    use boxpilot_helper::acl::sid;
    use boxpilot_helper::manifest;
    use boxpilot_helper::paths::{self, Layout};
    use boxpilot_helper::runcfg;
    use boxpilot_helper::rundir::RunDir;
    use boxpilot_helper::spawnplan;
    use boxpilot_helper::tokenplan::{
        integrity, same_privilege, ObservedToken, TokenPlan, NEVER_FOR_SING_BOX, SING_BOX_TOKEN,
    };
    use boxpilot_helper::win::probe as hooks;
    use boxpilot_protocol::{StartRequest, TunOptions};
    use serde_json::json;
    use std::fs::{self, File};
    use std::io::{self, BufRead, BufReader, Write};
    use std::net::{
        IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, ToSocketAddrs,
    };
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant};
    use windows::core::PCWSTR;
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        SetupUninstallOEMInfW, SUOI_FORCEDELETE,
    };
    use windows::Win32::Foundation::{ERROR_SERVICE_DOES_NOT_EXIST, WIN32_ERROR};
    use windows::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_HANDLE,
        SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STOPPED,
    };

    const USAGE: &str = "\
usage: token_probe --work <dir> [--budget-secs <seconds>] [--keep-driver]

Run as SYSTEM, on a machine you can throw away: it installs and removes
wintun's driver and brings TUN up and down many times. --budget-secs
(default 480) bounds the trials; the driver is put back whatever it says.
--keep-driver never removes wintun's driver package from the driver store
(so no first install is tried after the first trial): use it where another
program uses wintun (WireGuard, another sing-box client), whose package
removing would cut its tunnels.";

    /// The probe ran; what the trials found is in its files.
    const DONE: i32 = 0;
    /// The probe itself broke.
    const MALFUNCTION: i32 = 1;
    const USAGE_ERROR: i32 = 2;

    /// The default time for every trial together.
    const DEFAULT_BUDGET: Duration = Duration::from_secs(480);
    /// Past the budget by this much, the probe gives up as broken.
    const WATCHDOG_GRACE: Duration = Duration::from_secs(300);
    /// What one more trial may take at worst: no new trial starts with
    /// less left.
    const TRIAL_RESERVE: Duration = Duration::from_secs(45);
    /// What phase 4 needs: the removal, a reinstall, and room to put the
    /// driver back if that fails.
    const REINSTALL_RESERVE: Duration = Duration::from_secs(150);
    /// From the start to sing-box saying it and its TUN inbound started:
    /// the smoke client allows 45 s; a first install on CI took 1.4 s.
    const TUN_UP_TIMEOUT: Duration = Duration::from_secs(30);
    /// After sing-box ends, for its adapter to go and the route to return.
    const AFTER_TIMEOUT: Duration = Duration::from_secs(15);

    /// Where a probe connects: the smoke client's public address and name.
    const PUBLIC_ADDRESS: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 443);
    const PUBLIC_NAME: &str = "one.one.one.one";
    /// The probe profile's local rule set, as an attachment.
    const RULE_SET_ID: &str = "probe-rules";
    const RULE_SET: &str = r#"{"version": 1, "rules": [{"domain_suffix": ["probe.invalid"]}]}"#;

    pub fn main() -> i32 {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let (work, budget, keep_driver) = match parse(&args) {
            Ok(parsed) => parsed,
            Err(message) => {
                eprintln!("token_probe: {message}\n{USAGE}");
                return USAGE_ERROR;
            }
        };
        if let Err(error) = fs::create_dir_all(&work) {
            eprintln!("token_probe: {}: {error}", work.display());
            return MALFUNCTION;
        }
        match File::create(work.join("probe.log")) {
            Ok(file) => {
                let _ = LOG.set(Mutex::new(file));
            }
            Err(error) => {
                eprintln!("token_probe: {}: {error}", work.join("probe.log").display());
                return MALFUNCTION;
            }
        }
        let done = work.join("done");
        let watchdog_done = done.clone();
        thread::spawn(move || {
            thread::sleep(budget + WATCHDOG_GRACE);
            say(format!(
                "MALFUNCTION: still running {}s after the budget",
                WATCHDOG_GRACE.as_secs()
            ));
            let _ = write_atomic(
                &watchdog_done,
                "malfunction: the probe hung past its budget\n",
            );
            std::process::exit(MALFUNCTION);
        });
        let (code, status) = match run(&work, budget, keep_driver) {
            Ok(()) => (DONE, "ok\n".to_owned()),
            Err(message) => {
                say(format!("MALFUNCTION: {message}"));
                (MALFUNCTION, format!("malfunction: {message}\n"))
            }
        };
        if let Err(error) = write_atomic(&done, &status) {
            eprintln!("token_probe: {}: {error}", done.display());
            return MALFUNCTION;
        }
        code
    }

    fn parse(args: &[String]) -> Result<(PathBuf, Duration, bool), String> {
        let mut work = None;
        let mut budget = DEFAULT_BUDGET;
        let mut keep_driver = false;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let mut value = || {
                args.next()
                    .cloned()
                    .ok_or_else(|| format!("{arg} needs a value"))
            };
            match arg.as_str() {
                "--work" => work = Some(PathBuf::from(value()?)),
                "--budget-secs" => {
                    let secs: u64 = value()?
                        .parse()
                        .map_err(|_| "--budget-secs takes a number of seconds".to_owned())?;
                    budget = Duration::from_secs(secs);
                }
                "--keep-driver" => keep_driver = true,
                other => return Err(format!("unexpected argument {other:?}")),
            }
        }
        Ok((work.ok_or("--work is required")?, budget, keep_driver))
    }

    // ---- The log ----

    static LOG: OnceLock<Mutex<File>> = OnceLock::new();

    /// One line to stdout and to `probe.log`, flushed: a probe that dies
    /// leaves everything up to that point.
    fn say(line: impl AsRef<str>) {
        let line = line.as_ref();
        println!("{line}");
        if let Some(file) = LOG.get() {
            let mut file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }

    macro_rules! say {
        ($($arg:tt)*) => {
            say(format!($($arg)*))
        };
    }

    /// Written beside and renamed, so a reader never sees half.
    fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
        let partial = path.with_extension("partial");
        fs::write(&partial, text)?;
        fs::rename(&partial, path)
    }

    // ---- Tokens, as the probe plans and prints them ----

    /// A token to try: the privileges kept (as the probe's own token names
    /// them), and the two narrowings.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Candidate {
        keep: Vec<String>,
        high: bool,
        deny_admins: bool,
    }

    impl Candidate {
        fn privileges(keep: Vec<String>) -> Self {
            Self {
                keep,
                high: false,
                deny_admins: false,
            }
        }

        fn with(&self, high: bool, deny_admins: bool) -> Self {
            Self {
                keep: self.keep.clone(),
                high,
                deny_admins,
            }
        }

        /// The plan `win::probe::start` takes; `refs` are `keep` borrowed.
        fn plan<'a>(&self, refs: &'a [&'a str]) -> TokenPlan<'a> {
            TokenPlan {
                privileges: refs,
                max_integrity: self.high.then_some(integrity::HIGH),
                deny_only: if self.deny_admins {
                    &[sid::ADMINISTRATORS]
                } else {
                    &[]
                },
            }
        }

        fn narrowings(&self) -> String {
            match (self.high, self.deny_admins) {
                (false, false) => String::new(),
                (true, false) => " +IL High".into(),
                (false, true) => " +Administrators deny-only".into(),
                (true, true) => " +IL High +Administrators deny-only".into(),
            }
        }

        /// Short enough for one table cell.
        fn describe(&self, held: &[String]) -> String {
            let missing: Vec<&String> = held
                .iter()
                .filter(|name| !self.keep.iter().any(|kept| same_privilege(kept, name)))
                .collect();
            let privileges = if missing.is_empty() {
                "every privilege".to_owned()
            } else if self.keep.len() <= 4 {
                self.keep
                    .iter()
                    .map(|name| short(name))
                    .collect::<Vec<_>>()
                    .join(",")
            } else if missing.len() <= 6 {
                format!(
                    "all but {}",
                    missing
                        .iter()
                        .map(|name| short(name))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            } else if missing.iter().all(|name| is_never(name)) {
                format!("all but {} of NEVER_FOR_SING_BOX", missing.len())
            } else {
                format!("{} privileges", self.keep.len())
            };
            privileges + &self.narrowings()
        }
    }

    /// `SeLoadDriverPrivilege` as `LoadDriver`.
    fn short(name: &str) -> String {
        name.strip_prefix("Se")
            .and_then(|rest| rest.strip_suffix("Privilege"))
            .unwrap_or(name)
            .to_owned()
    }

    fn is_never(name: &str) -> bool {
        NEVER_FOR_SING_BOX
            .iter()
            .any(|never| same_privilege(never, name))
    }

    /// The ladder, smallest first, each rung holding every one below it,
    /// spelled as `held` spells them: `SeChangeNotifyPrivilege`; then
    /// `SeLoadDriverPrivilege`; then every held privilege not in
    /// `NEVER_FOR_SING_BOX`; then impersonation; then the ones that read,
    /// write or relabel any file; then everything held.
    fn rungs(held: &[String]) -> Vec<Candidate> {
        let pick = |names: &[&str]| -> Vec<String> {
            held.iter()
                .filter(|name| names.iter().any(|wanted| same_privilege(wanted, name)))
                .cloned()
                .collect()
        };
        let groups: Vec<Vec<String>> = vec![
            pick(&["SeChangeNotifyPrivilege"]),
            pick(&["SeLoadDriverPrivilege"]),
            held.iter()
                .filter(|name| !is_never(name))
                .cloned()
                .collect(),
            pick(&[
                "SeImpersonatePrivilege",
                "SeDelegateSessionUserImpersonatePrivilege",
            ]),
            pick(&[
                "SeBackupPrivilege",
                "SeRestorePrivilege",
                "SeTakeOwnershipPrivilege",
                "SeSecurityPrivilege",
                "SeManageVolumePrivilege",
                "SeRelabelPrivilege",
            ]),
            held.to_vec(),
        ];
        let mut keep: Vec<String> = Vec::new();
        let mut rungs: Vec<Candidate> = Vec::new();
        for group in groups {
            for name in group {
                if !keep.contains(&name) {
                    keep.push(name);
                }
            }
            if rungs.last().is_none_or(|last| last.keep.len() < keep.len()) && !keep.is_empty() {
                rungs.push(Candidate::privileges(keep.clone()));
            }
        }
        rungs
    }

    /// The order to try dropping privileges from a working set: the
    /// dangerous ones first, `SeLoadDriverPrivilege` last, never
    /// `SeChangeNotifyPrivilege`.
    fn drop_order(keep: &[String]) -> Vec<String> {
        let mut order: Vec<String> = keep.iter().filter(|name| is_never(name)).cloned().collect();
        let mut rest: Vec<String> = keep
            .iter()
            .filter(|name| {
                !is_never(name)
                    && !same_privilege(name, "SeChangeNotifyPrivilege")
                    && !same_privilege(name, "SeLoadDriverPrivilege")
            })
            .cloned()
            .collect();
        rest.sort();
        order.extend(rest);
        order.extend(
            keep.iter()
                .filter(|name| same_privilege(name, "SeLoadDriverPrivilege"))
                .cloned(),
        );
        order
    }

    fn integrity_name(level: Option<u32>) -> String {
        match level {
            Some(integrity::SYSTEM) => "System".into(),
            Some(integrity::HIGH) => "High".into(),
            Some(integrity::MEDIUM) => "Medium".into(),
            Some(other) => format!("0x{other:x}"),
            None => "?".into(),
        }
    }

    fn privilege_attributes(attributes: u32) -> &'static str {
        // SE_PRIVILEGE_ENABLED_BY_DEFAULT 1, SE_PRIVILEGE_ENABLED 2.
        match attributes & 3 {
            3 => "enabled (by default)",
            2 => "enabled",
            1 => "disabled (enabled by default)",
            _ => "disabled",
        }
    }

    /// How the token treats Administrators: `enabled`, `deny-only`, or
    /// `absent`.
    fn administrators(token: &ObservedToken) -> String {
        match token
            .groups
            .iter()
            .find(|(group, _)| group.eq_ignore_ascii_case(sid::ADMINISTRATORS))
        {
            None => "absent".into(),
            Some((_, attributes)) if attributes & 0x10 != 0 => "deny-only".into(),
            Some((_, attributes)) if attributes & 0x4 != 0 => "enabled".into(),
            Some((_, attributes)) => format!("0x{attributes:x}"),
        }
    }

    fn print_token(what: &str, token: &ObservedToken) {
        say!(
            "{what}: integrity {}, Administrators {}, {} privileges:",
            integrity_name(token.integrity),
            administrators(token),
            token.privileges.len()
        );
        for (name, attributes) in &token.privileges {
            say!("    {name:<44} {}", privilege_attributes(*attributes));
        }
    }

    // ---- The machine: wintun's driver, adapters, the network ----

    /// A service-control handle, closed on drop.
    struct ScHandle(SC_HANDLE);

    impl Drop for ScHandle {
        fn drop(&mut self) {
            // SAFETY: an open handle that only this value owns.
            let _ = unsafe { CloseServiceHandle(self.0) };
        }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The `wintun` driver service: `absent`, `stopped` (installed, not
    /// loaded), `running` (loaded), or what went wrong.
    fn driver_service() -> String {
        // SAFETY: the local machine's active database; the handle is owned.
        let manager =
            match unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) } {
                Ok(manager) => ScHandle(manager),
                Err(error) => return format!("? ({error})"),
            };
        let name = wide("wintun");
        // SAFETY: an open manager handle and a NUL-terminated name; the
        // handle returned is owned.
        let service = match unsafe {
            OpenServiceW(manager.0, PCWSTR(name.as_ptr()), SERVICE_QUERY_STATUS)
        } {
            Ok(service) => ScHandle(service),
            Err(error) if WIN32_ERROR::from_error(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
                return "absent".into()
            }
            Err(error) => return format!("? ({error})"),
        };
        let mut status = SERVICE_STATUS::default();
        // SAFETY: an open service handle with SERVICE_QUERY_STATUS.
        if let Err(error) = unsafe { QueryServiceStatus(service.0, &mut status) } {
            return format!("? ({error})");
        }
        if status.dwCurrentState == SERVICE_RUNNING {
            "running".into()
        } else if status.dwCurrentState == SERVICE_STOPPED {
            "stopped".into()
        } else {
            format!("state {}", status.dwCurrentState.0)
        }
    }

    /// The published names (`oem12.inf`) of the driver packages in the
    /// store that are wintun's: their INF copy names `wintun.sys`.
    fn wintun_packages(system_root: &str) -> Vec<String> {
        let Ok(entries) = fs::read_dir(Path::new(system_root).join("INF")) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let lower = name.to_ascii_lowercase();
            if !(lower.starts_with("oem") && lower.ends_with(".inf")) {
                continue;
            }
            let Ok(bytes) = fs::read(entry.path()) else {
                continue;
            };
            let text = if bytes.starts_with(&[0xff, 0xfe]) {
                let units: Vec<u16> = bytes[2..]
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .collect();
                String::from_utf16_lossy(&units)
            } else {
                String::from_utf8_lossy(&bytes).into_owned()
            };
            if text.to_ascii_lowercase().contains("wintun.sys") {
                found.push(name);
            }
        }
        found.sort();
        found
    }

    fn driver_state(system_root: &str) -> String {
        let packages = wintun_packages(system_root);
        format!(
            "service {}, store {}",
            driver_service(),
            if packages.is_empty() {
                "none".to_owned()
            } else {
                packages.join("+")
            }
        )
    }

    /// Remove wintun's driver package from the driver store (and its INF),
    /// so the next adapter installs it again: a first install.
    fn remove_wintun_packages(system_root: &str) -> Result<Vec<String>, String> {
        let packages = wintun_packages(system_root);
        for package in &packages {
            let name = wide(package);
            // SAFETY: `name` is NUL-terminated and outlives the call; no
            // reserved pointer.
            let removed =
                unsafe { SetupUninstallOEMInfW(PCWSTR(name.as_ptr()), SUOI_FORCEDELETE, None) };
            if !removed.as_bool() {
                return Err(format!(
                    "SetupUninstallOEMInfW({package}): {}",
                    io::Error::last_os_error()
                ));
            }
        }
        Ok(packages)
    }

    fn adapters_text() -> String {
        let adapters = hooks::sing_tun_adapters();
        if adapters.is_empty() {
            return "none".into();
        }
        adapters
            .iter()
            .map(|(name, present)| {
                format!(
                    "{name} ({})",
                    if *present { "present" } else { "not present" }
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn tun_address() -> Ipv4Addr {
        boxpilot_runconfig::TUN_IPV4_ADDRESS
            .split('/')
            .next()
            .and_then(|address| address.parse().ok())
            .expect("TUN_IPV4_ADDRESS is an IPv4 address with a prefix length")
    }

    /// Clear the DNS client's cache, so a name resolves afresh.
    fn flush_dns(system_root: &str) {
        let ipconfig = Path::new(system_root).join(r"System32\ipconfig.exe");
        let _ = Command::new(ipconfig)
            .arg("/flushdns")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    /// This machine's own connection to the internet: the local address it
    /// leaves from.
    fn connect_out(timeout: Duration) -> Result<SocketAddr, String> {
        TcpStream::connect_timeout(&PUBLIC_ADDRESS.into(), timeout)
            .and_then(|stream| stream.local_addr())
            .map_err(|error| format!("{PUBLIC_ADDRESS}: {error}"))
    }

    /// Through TUN: the connection leaves from the TUN address.
    fn tcp_via_tun() -> Result<String, String> {
        let local = connect_out(Duration::from_secs(8))?;
        if local.ip() == IpAddr::V4(tun_address()) {
            Ok(format!("from {local}"))
        } else {
            Err(format!("left from {local}, not the TUN address"))
        }
    }

    fn resolve(system_root: &str) -> Result<String, String> {
        flush_dns(system_root);
        match (PUBLIC_NAME, 443).to_socket_addrs() {
            Ok(mut addresses) => addresses
                .next()
                .map(|address| address.ip().to_string())
                .ok_or_else(|| format!("{PUBLIC_NAME} resolved to nothing")),
            Err(error) => Err(format!("{PUBLIC_NAME}: {error}")),
        }
    }

    /// `check` until it passes, at most `tries` times a second apart.
    fn retry(
        tries: u32,
        mut check: impl FnMut() -> Result<String, String>,
    ) -> Result<String, String> {
        let mut last = Err(String::new());
        for attempt in 0..tries {
            if attempt > 0 {
                thread::sleep(Duration::from_secs(1));
            }
            last = check();
            if last.is_ok() {
                break;
            }
        }
        last
    }

    /// Whether `done` holds within `timeout`, looked at every 250 ms.
    fn within(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if done() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(250));
        }
    }

    fn free_port() -> Result<u16, String> {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|listener| listener.local_addr())
            .map(|address| address.port())
            .map_err(|error| format!("no free loopback port: {error}"))
    }

    /// The smoke client's TUN profile (`service_smoke::tun_start`), with
    /// its local rule set attached as the GUI attaches it.
    fn start_request(proxy_port: u16) -> Result<StartRequest, String> {
        let mut config = json!({
            "log": {"level": "info"},
            "dns": {
                "servers": [{"type": "udp", "tag": "public", "server": "1.1.1.1"}]
            },
            "outbounds": [{"type": "direct", "tag": "direct"}],
            "route": {
                "rule_set": [{
                    "type": "local",
                    "tag": "probe",
                    "format": "source",
                    "path": "probe-rules.json"
                }],
                "rules": [
                    {"action": "sniff"},
                    {"protocol": "dns", "action": "hijack-dns"},
                    {"rule_set": "probe", "action": "reject"}
                ],
                "auto_detect_interface": true,
                "default_domain_resolver": "public",
                "final": "direct"
            }
        });
        let fields = boxpilot_policy::local_file_fields(&config);
        let [field] = fields.as_slice() else {
            return Err(format!(
                "the probe profile reads {} local files, not 1",
                fields.len()
            ));
        };
        boxpilot_policy::attach(&mut config, &field.pointer, RULE_SET_ID)
            .map_err(|error| format!("attaching {}: {error}", field.pointer))?;
        Ok(StartRequest {
            config: config.to_string(),
            attachments: vec![(RULE_SET_ID.to_owned(), RULE_SET.as_bytes().to_vec())],
            options: TunOptions {
                ipv6: false,
                proxy_port,
                allow_lan: false,
                system_proxy: false,
            },
        })
    }

    // ---- Trials ----

    /// What one trial saw.
    #[derive(Debug, Default)]
    struct Outcome {
        /// Why sing-box didn't start at all (the token or the spawn).
        start_error: Option<String>,
        /// sing-box's token, read from its process.
        token: Option<ObservedToken>,
        /// It exited on its own, with this code.
        exited: Option<u32>,
        tun_up: bool,
        started: bool,
        tcp: Option<Result<String, String>>,
        dns: Option<Result<String, String>>,
        adapter_gone: bool,
        route_back: bool,
        stale_removed: u32,
        /// sing-box's first error lines.
        errors: Vec<String>,
        driver_before: String,
        driver_after: String,
        secs: f64,
    }

    impl Outcome {
        fn works(&self) -> bool {
            self.start_error.is_none()
                && self.tun_up
                && self.started
                && matches!(self.tcp, Some(Ok(_)))
                && matches!(self.dns, Some(Ok(_)))
                && self.adapter_gone
        }

        /// TUN came up, but something after it failed: worth one retry
        /// before a privilege is judged needed.
        fn came_up(&self) -> bool {
            self.start_error.is_none() && self.tun_up && self.started
        }

        fn verdict(&self) -> String {
            if self.works() {
                return "WORKS".into();
            }
            if let Some(error) = &self.start_error {
                return format!("not started: {error}");
            }
            if !(self.tun_up && self.started) {
                let first = self
                    .errors
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "no TUN within the timeout".into());
                return match self.exited {
                    Some(code) => format!("exited {code}: {first}"),
                    None => first,
                };
            }
            if let Some(Err(error)) = &self.tcp {
                return format!("no TCP through TUN: {error}");
            }
            if let Some(Err(error)) = &self.dns {
                return format!("no DNS: {error}");
            }
            "its adapter stayed after it ended".into()
        }
    }

    struct Trial {
        n: usize,
        phase: &'static str,
        candidate: Candidate,
        outcome: Outcome,
    }

    /// A run directory with its config, and sing-box's command line and
    /// environment for it.
    struct PreparedRun {
        run: RunDir,
        args: Vec<String>,
        environment: Vec<(String, String)>,
    }

    struct Probe {
        work: PathBuf,
        sing_box: PathBuf,
        system_root: String,
        /// What the probe's own token (SYSTEM's) holds.
        held: Vec<String>,
        /// `--keep-driver`: never remove wintun's driver package.
        keep_driver: bool,
        deadline: Instant,
        trials: Vec<Trial>,
    }

    impl Probe {
        fn time_for(&self, reserve: Duration) -> bool {
            Instant::now() + reserve <= self.deadline
        }

        /// A run directory with the config the helper would write for the
        /// probe profile, in `<work>\runs\trial-<n>`, and sing-box's
        /// arguments and environment for it. Failing here is the probe's
        /// fault.
        fn prepare_run(&self, n: usize) -> Result<PreparedRun, String> {
            let fail = |what: &str, error: &dyn std::fmt::Display| format!("{what}: {error}");
            let runs = self.work.join("runs");
            let users = self.work.join("users").join(sid::SYSTEM);
            fs::create_dir_all(&runs).map_err(|error| fail("the runs directory", &error))?;
            fs::create_dir_all(&users).map_err(|error| fail("the users directory", &error))?;
            let path = runs.join(format!("trial-{n}"));
            if path.exists() {
                fs::remove_dir_all(&path).map_err(|error| fail("an old run directory", &error))?;
            }
            fs::create_dir(&path).map_err(|error| fail("the run directory", &error))?;
            let run = RunDir::adopt(path);
            let placement =
                paths::placement(run.path(), &users).map_err(|error| fail("placement", &error))?;
            let checked = runcfg::check(start_request(free_port()?)?).map_err(|refusals| {
                format!("the policy refuses the probe profile: {refusals:?}")
            })?;
            let secret = runcfg::fresh_secret().map_err(|error| fail("the OS RNG", &error))?;
            let prepared = runcfg::build(checked, &placement, runcfg::free_loopback_port, &secret)
                .map_err(|error| fail("the run config", &error))?;
            run.write(&prepared)
                .map_err(|error| fail("writing the run config", &error))?;
            let temp = run
                .create_dir("tmp")
                .map_err(|error| fail("TEMP", &error))?;
            let home = run
                .create_dir("home")
                .map_err(|error| fail("USERPROFILE", &error))?;
            let text = |path: &Path| -> Result<String, String> {
                path.to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{} is not Unicode", path.display()))
            };
            let args = spawnplan::sing_box_args(&text(run.path())?, &text(&run.config_path())?);
            let environment =
                spawnplan::environment(&self.system_root, &text(&temp)?, &text(&home)?);
            Ok(PreparedRun {
                run,
                args,
                environment,
            })
        }

        /// One trial of `candidate`: whether TUN worked. `Err` only when
        /// the probe itself broke.
        fn trial(&mut self, phase: &'static str, candidate: &Candidate) -> Result<bool, String> {
            let n = self.trials.len() + 1;
            say!("");
            say!(
                "==== trial {n} ({phase}): {}",
                candidate.describe(&self.held)
            );
            say!("keeps: {}", candidate.keep.join(" "));
            let began = Instant::now();
            let mut outcome = Outcome::default();
            let removed = hooks::remove_stale_sing_tun_adapters();
            if removed > 0 {
                say!("removed {removed} stale sing-tun adapter(s) first");
            }
            flush_dns(&self.system_root);
            outcome.driver_before = driver_state(&self.system_root);
            say!("wintun driver before: {}", outcome.driver_before);

            let PreparedRun {
                run: run_dir,
                args,
                environment,
            } = self.prepare_run(n)?;
            let refs: Vec<&str> = candidate.keep.iter().map(String::as_str).collect();
            let plan = candidate.plan(&refs);
            match hooks::start(&self.sing_box, args, run_dir.path(), environment, &plan) {
                Err(error) => {
                    say!("sing-box did not start: {error}");
                    outcome.start_error = Some(error.to_string());
                }
                Ok((run, output)) => self.watch(&run, output, &mut outcome),
            }

            outcome.adapter_gone = within(AFTER_TIMEOUT, || {
                hooks::sing_tun_adapters()
                    .iter()
                    .all(|(_, present)| !present)
            });
            outcome.route_back = within(AFTER_TIMEOUT, || {
                connect_out(Duration::from_secs(3))
                    .is_ok_and(|local| local.ip() != IpAddr::V4(tun_address()))
            });
            say!(
                "after: adapters {}; adapter gone {}, own route back {}",
                adapters_text(),
                outcome.adapter_gone,
                outcome.route_back
            );
            outcome.stale_removed = hooks::remove_stale_sing_tun_adapters();
            outcome.driver_after = driver_state(&self.system_root);
            say!(
                "removed {} stale adapter(s); wintun driver after: {}",
                outcome.stale_removed,
                outcome.driver_after
            );
            drop(run_dir);
            outcome.secs = began.elapsed().as_secs_f64();
            let works = outcome.works();
            say!("trial {n}: {} ({:.1} s)", outcome.verdict(), outcome.secs);
            self.trials.push(Trial {
                n,
                phase,
                candidate: candidate.clone(),
                outcome,
            });
            Ok(works)
        }

        /// A trial of a first install: whether the driver installed and the
        /// adapter came up (sing-box logged its TUN inbound and itself
        /// started). Whether traffic then flows is phase 2's question; the
        /// table shows both.
        fn trial_installs(
            &mut self,
            phase: &'static str,
            candidate: &Candidate,
        ) -> Result<bool, String> {
            self.trial(phase, candidate)?;
            Ok(self
                .trials
                .last()
                .is_some_and(|trial| trial.outcome.came_up()))
        }

        /// A trial whose TUN came up but failed after (traffic, DNS, the
        /// adapter) gets one more go, so a slow answer from the internet
        /// isn't read as a privilege TUN needs.
        fn trial_twice(
            &mut self,
            phase: &'static str,
            candidate: &Candidate,
        ) -> Result<bool, String> {
            if self.trial(phase, candidate)? {
                return Ok(true);
            }
            let came_up = self
                .trials
                .last()
                .is_some_and(|trial| trial.outcome.came_up());
            if came_up && self.time_for(TRIAL_RESERVE) {
                say!("TUN came up but a check after it failed: once more");
                return self.trial(phase, candidate);
            }
            Ok(false)
        }

        /// While sing-box runs: its token, its output, TUN coming up, and
        /// traffic through it; then end it as the helper does.
        fn watch(&self, run: &hooks::Run, output: hooks::Output, outcome: &mut Outcome) {
            say!("sing-box started, pid {}", run.pid());
            match run.token() {
                Ok(token) => {
                    print_token("its token", &token);
                    outcome.token = Some(token);
                }
                Err(error) => say!("its token could not be read: {error}"),
            }
            let lines: Arc<Mutex<Vec<String>>> = Arc::default();
            let readers: Vec<_> = [output.stdout, output.stderr]
                .into_iter()
                .map(|pipe| {
                    let lines = lines.clone();
                    thread::spawn(move || {
                        let mut reader = BufReader::new(pipe);
                        let mut buf = Vec::new();
                        loop {
                            buf.clear();
                            match reader.read_until(b'\n', &mut buf) {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {
                                    let line = String::from_utf8_lossy(&buf).trim_end().to_owned();
                                    let line: String = line.chars().take(400).collect();
                                    say!("  sing-box | {line}");
                                    lines
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                                        .push(line);
                                }
                            }
                        }
                    })
                })
                .collect();

            let deadline = Instant::now() + TUN_UP_TIMEOUT;
            loop {
                {
                    let lines = lines
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    outcome.tun_up = lines
                        .iter()
                        .any(|line| line.contains("inbound/tun") && line.contains("started"));
                    outcome.started = lines.iter().any(|line| line.contains("sing-box started"));
                }
                if outcome.tun_up && outcome.started {
                    break;
                }
                match run.wait(Duration::ZERO) {
                    Ok(Some(code)) => {
                        outcome.exited = Some(code);
                        break;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        say!("waiting for sing-box: {error}");
                        break;
                    }
                }
                if Instant::now() >= deadline {
                    say!("no TUN within {}s", TUN_UP_TIMEOUT.as_secs());
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            if outcome.tun_up && outcome.started {
                let tcp = retry(3, tcp_via_tun);
                say!("TCP through TUN: {tcp:?}");
                outcome.tcp = Some(tcp);
                let dns = retry(3, || resolve(&self.system_root));
                say!("DNS: {dns:?}");
                outcome.dns = Some(dns);
                if let Ok(Some(code)) = run.wait(Duration::ZERO) {
                    say!("sing-box exited ({code}) while it was checked");
                    outcome.exited = Some(code);
                }
            }
            run.stop();
            match run.wait(Duration::from_secs(15)) {
                Ok(Some(code)) => say!("sing-box ended, exit code {code}"),
                Ok(None) => say!("sing-box still runs 15 s after its job was ended"),
                Err(error) => say!("waiting for sing-box to end: {error}"),
            }
            for reader in readers {
                let _ = reader.join();
            }
            outcome.errors = lines
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .filter(|line| line.contains("FATAL") || line.contains("ERROR"))
                .take(4)
                .cloned()
                .collect();
        }
    }

    // ---- The run ----

    fn run(work: &Path, budget: Duration, keep_driver: bool) -> Result<(), String> {
        let began = Instant::now();
        say!("token_probe: which token sing-box needs for TUN, measured (ADR 0006)");
        let (user, own) =
            hooks::own_token().map_err(|error| format!("reading this process's token: {error}"))?;
        say!("running as {user}");
        print_token("this process's token (what SYSTEM holds here)", &own);
        say!("its groups:");
        for (group, attributes) in &own.groups {
            say!("    {group:<60} 0x{attributes:08x}");
        }
        if user != sid::SYSTEM {
            return Err(format!(
                "this runs as {user}, not SYSTEM: run it from a SYSTEM scheduled task, as the \
                 helper runs (helper-smoke.ps1 -Step token-probe)"
            ));
        }
        let held: Vec<String> = own
            .privileges
            .iter()
            .map(|(name, _)| name.clone())
            .collect();

        let program_files =
            hooks::program_files().map_err(|error| format!("Program Files: {error}"))?;
        let layout = Layout::installed(&program_files);
        let manifest_path = layout.manifest_file();
        let bytes = fs::read(&manifest_path)
            .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
        let installed = manifest::parse(&bytes)
            .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
        let sing_box = layout.helper_file(&installed.sing_box.file);
        if !sing_box.is_file() {
            return Err(format!(
                "{} is not there: install the MSI first",
                sing_box.display()
            ));
        }
        let system_root =
            hooks::windows_dir().map_err(|error| format!("the Windows directory: {error}"))?;
        say!(
            "sing-box {}: {}",
            installed.sing_box.version,
            sing_box.display()
        );
        say!(
            "before any trial: wintun driver {}; sing-tun adapters {}",
            driver_state(&system_root),
            adapters_text()
        );

        let mut probe = Probe {
            work: work.to_owned(),
            sing_box,
            system_root,
            held: held.clone(),
            keep_driver,
            deadline: began + budget,
            trials: Vec::new(),
        };
        let rungs = rungs(&held);
        let findings = phases(&mut probe, &rungs)?;
        report(&probe, &findings, &own)?;
        if findings.install.is_none() && findings.all_rungs_tried {
            return Err(
                "TUN never came up, not even with every privilege SYSTEM holds: the probe or \
                 this machine is at fault, not the token (each trial's output is above)"
                    .into(),
            );
        }
        Ok(())
    }

    /// What the phases found.
    #[derive(Default)]
    struct Findings {
        /// The first rung that installed the driver and brought the
        /// adapter up.
        install: Option<Candidate>,
        /// Phase 1 tried every rung (and none worked, if `install` is None).
        all_rungs_tried: bool,
        /// The smallest set found for steady state.
        steady: Option<Candidate>,
        /// Every privilege was tried for dropping.
        steady_complete: bool,
        high: Option<bool>,
        deny_admins: Option<bool>,
        both: Option<bool>,
        /// The strictest token that worked in steady state.
        strictest: Option<Candidate>,
        /// Phase 4: the shipped plan installed the driver on a first install
        /// (the adapter came up), and fully worked in steady state.
        shipped_install: Option<bool>,
        shipped_steady: Option<bool>,
        /// Phase 5: the strictest token, and whether it installed the
        /// driver.
        reinstall: Option<(Candidate, bool)>,
        /// Phase 5: the smallest token found that installs the driver.
        install_needs: Option<Candidate>,
    }

    impl Findings {
        /// The regression check: the shipped plan installs the driver and
        /// carries traffic. `None` when it wasn't (fully) tried.
        fn shipped(&self) -> Option<bool> {
            match (self.shipped_install, self.shipped_steady) {
                (Some(install), Some(steady)) => Some(install && steady),
                (Some(false), None) | (None, Some(false)) => Some(false),
                _ => None,
            }
        }
    }

    fn phases(probe: &mut Probe, rungs: &[Candidate]) -> Result<Findings, String> {
        let mut findings = Findings::default();

        say!("");
        say!("######## 1. first install: wintun's driver, smallest token first");
        findings.all_rungs_tried = true;
        for rung in rungs {
            if !probe.time_for(TRIAL_RESERVE) {
                findings.all_rungs_tried = false;
                break;
            }
            if probe.trial_installs("install", rung)? {
                findings.install = Some(rung.clone());
                break;
            }
        }
        let Some(install) = findings.install.clone() else {
            return Ok(findings);
        };

        // The driver unloads once no adapter is left; whether it did says
        // whether the next trial loads it again.
        let unloaded = within(Duration::from_secs(15), || driver_service() == "stopped");
        say!("");
        say!(
            "after the install: wintun driver {} ({})",
            driver_state(&probe.system_root),
            if unloaded {
                "unloaded again: the next trial loads it"
            } else {
                "still loaded"
            }
        );

        say!("");
        say!("######## 2. steady state: the smallest set that works");
        let mut working: Option<Candidate> = None;
        for rung in rungs {
            if !probe.time_for(TRIAL_RESERVE) {
                break;
            }
            if probe.trial_twice("steady", rung)? {
                working = Some(rung.clone());
                break;
            }
        }
        if let Some(found) = working {
            let mut keep = found.keep.clone();
            findings.steady_complete = true;
            for name in drop_order(&keep) {
                if !probe.time_for(TRIAL_RESERVE) {
                    findings.steady_complete = false;
                    break;
                }
                let without: Vec<String> =
                    keep.iter().filter(|kept| **kept != name).cloned().collect();
                if probe.trial_twice("drop", &Candidate::privileges(without.clone()))? {
                    say!("{name}: not needed");
                    keep = without;
                } else {
                    say!("{name}: needed");
                }
            }
            findings.steady = Some(Candidate::privileges(keep));
        }
        let Some(steady) = findings.steady.clone() else {
            return Ok(findings);
        };

        say!("");
        say!("######## 3. narrowings on {}", steady.describe(&probe.held));
        let try_narrowing =
            |probe: &mut Probe, high: bool, deny: bool| -> Result<Option<bool>, String> {
                if !probe.time_for(TRIAL_RESERVE) {
                    return Ok(None);
                }
                probe
                    .trial_twice("narrow", &steady.with(high, deny))
                    .map(Some)
            };
        findings.high = try_narrowing(probe, true, false)?;
        findings.deny_admins = try_narrowing(probe, false, true)?;
        findings.both = try_narrowing(probe, true, true)?;
        let strictest = match (findings.high, findings.deny_admins, findings.both) {
            (_, _, Some(true)) => steady.with(true, true),
            (Some(true), _, _) => steady.with(true, false),
            (_, Some(true), _) => steady.with(false, true),
            _ => steady.clone(),
        };
        findings.strictest = Some(strictest.clone());

        let full = rungs.last().unwrap_or(&install).clone();
        let shipped = shipped_candidate(&probe.held);

        say!("");
        say!(
            "######## 4. the shipped plan, tokenplan::SING_BOX_TOKEN ({}): a first install, then \
             steady state",
            shipped.describe(&probe.held)
        );
        if probe.time_for(REINSTALL_RESERVE) && fresh_install(probe) {
            let installed = probe.trial_installs("shipped", &shipped)?;
            findings.shipped_install = Some(installed);
            if !installed {
                restore_driver(probe, &install, &full)?;
            }
        } else {
            say!("its first install is untested: no time left, or the driver package stayed");
        }
        if probe.time_for(TRIAL_RESERVE) {
            findings.shipped_steady = Some(probe.trial_twice("shipped", &shipped)?);
        } else {
            say!("skipped: not enough time left in the budget");
        }

        say!("");
        if strictest == shipped {
            say!("######## 5. the strictest token that worked is the shipped plan: nothing more to install");
        } else {
            say!(
                "######## 5. install again, with {}",
                strictest.describe(&probe.held)
            );
            if probe.time_for(REINSTALL_RESERVE) {
                reinstall(probe, &mut findings, &strictest, &install, &full)?;
            } else {
                say!("skipped: not enough time left in the budget");
            }
        }
        Ok(findings)
    }

    /// Remove wintun's driver package once its driver has unloaded, so
    /// the next trial is a first install; whether it was removed.
    fn fresh_install(probe: &Probe) -> bool {
        if probe.keep_driver {
            say!("wintun's driver package stays (--keep-driver): no first install");
            return false;
        }
        let unloaded = within(Duration::from_secs(15), || driver_service() != "running");
        match remove_wintun_packages(&probe.system_root) {
            Ok(removed) => {
                say!(
                    "removed wintun's driver package ({}){}; now: {}",
                    if removed.is_empty() {
                        "there was none".to_owned()
                    } else {
                        removed.join(", ")
                    },
                    if unloaded {
                        ""
                    } else {
                        ", the driver still loaded"
                    },
                    driver_state(&probe.system_root)
                );
                true
            }
            Err(error) => {
                say!("wintun's driver package could not be removed: {error}");
                false
            }
        }
    }

    /// Phase 5, when a token narrower than the shipped one worked in steady
    /// state: a first install again with `strictest`, the strictest such
    /// token (judged as phase 1 judges: the driver installed and the
    /// adapter came up). If it can't install the driver: with the
    /// privileges of `install` (phase 1's) added, with and then without the
    /// narrowings, and then which of those added privileges a first install
    /// needs, each trial a first install again. Whatever happens, the
    /// driver is installed again at the end.
    fn reinstall(
        probe: &mut Probe,
        findings: &mut Findings,
        strictest: &Candidate,
        install: &Candidate,
        full: &Candidate,
    ) -> Result<(), String> {
        if !fresh_install(probe) {
            return Ok(());
        }
        let mut installed = probe.trial_installs("reinstall", strictest)?;
        findings.reinstall = Some((strictest.clone(), installed));
        if installed {
            findings.install_needs = Some(strictest.clone());
            return Ok(());
        }

        let mut keep = strictest.keep.clone();
        for name in &install.keep {
            if !keep.contains(name) {
                keep.push(name.clone());
            }
        }
        let union = Candidate {
            keep,
            ..strictest.clone()
        };
        let mut base = None;
        for candidate in [union.clone(), union.with(false, false)] {
            if candidate == *strictest || !probe.time_for(TRIAL_RESERVE) || !fresh_install(probe) {
                continue;
            }
            if probe.trial_installs("reinstall", &candidate)? {
                installed = true;
                base = Some(candidate);
                break;
            }
            installed = false;
        }
        if let Some(base) = base {
            let mut keep = base.keep.clone();
            for name in drop_order(&keep) {
                if strictest.keep.contains(&name) {
                    continue;
                }
                if !probe.time_for(TRIAL_RESERVE) || !fresh_install(probe) {
                    break;
                }
                let without: Vec<String> =
                    keep.iter().filter(|kept| **kept != name).cloned().collect();
                let candidate = Candidate {
                    keep: without.clone(),
                    ..base.clone()
                };
                installed = probe.trial_installs("reinstall", &candidate)?;
                if installed {
                    say!("{name}: not needed to install");
                    keep = without;
                } else {
                    say!("{name}: needed to install");
                }
            }
            let needs = Candidate { keep, ..base };
            if !installed {
                installed = probe.trial_installs("restore", &needs)?;
            }
            findings.install_needs = Some(needs);
        }
        if !installed {
            restore_driver(probe, install, full)?;
        }
        Ok(())
    }

    /// Install wintun's driver again after a first install that failed:
    /// with `install` (phase 1's token), then `full`. It runs whatever the
    /// budget says, so the steps after the probe find the machine as phase 1
    /// left it.
    fn restore_driver(
        probe: &mut Probe,
        install: &Candidate,
        full: &Candidate,
    ) -> Result<(), String> {
        for candidate in [install, full] {
            if probe.trial_installs("restore", candidate)? {
                return Ok(());
            }
        }
        say!(
            "WARNING: wintun's driver could not be installed again; the next TUN start installs it"
        );
        Ok(())
    }

    /// `tokenplan::SING_BOX_TOKEN` as a candidate: its privileges as this
    /// token spells them. Its integrity cap can only be High and its
    /// deny-only group only Administrators: the two narrowings the probe
    /// knows.
    fn shipped_candidate(held: &[String]) -> Candidate {
        Candidate {
            keep: held
                .iter()
                .filter(|name| {
                    SING_BOX_TOKEN
                        .privileges
                        .iter()
                        .any(|kept| same_privilege(kept, name))
                })
                .cloned()
                .collect(),
            high: SING_BOX_TOKEN.max_integrity == Some(integrity::HIGH),
            deny_admins: !SING_BOX_TOKEN.deny_only.is_empty(),
        }
    }

    // ---- The report ----

    fn yes(value: bool) -> &'static str {
        if value {
            "yes"
        } else {
            "no"
        }
    }

    fn check(value: &Option<Result<String, String>>) -> &'static str {
        match value {
            None => "-",
            Some(Ok(_)) => "yes",
            Some(Err(_)) => "no",
        }
    }

    fn found(value: Option<bool>) -> &'static str {
        match value {
            None => "untested",
            Some(true) => "works",
            Some(false) => "fails",
        }
    }

    fn report(probe: &Probe, findings: &Findings, own: &ObservedToken) -> Result<(), String> {
        let mut out = String::new();
        let mut line = |text: String| {
            out.push_str(&text);
            out.push('\n');
        };
        line(format!(
            "SYSTEM's privileges here ({}): {}",
            own.privileges.len(),
            probe.held.join(" ")
        ));
        line(String::new());
        line(format!(
            "{:>3}  {:<9}  {:<52}  {:<6}  {:<9}  {:<15}  {:<15}  {:<2}  {:<3}  {:<3}  {:<4}  {:<5}  {:>5}  verdict",
            "#", "phase", "token", "IL", "admins", "driver before", "driver after", "up", "tcp", "dns", "gone", "route", "secs"
        ));
        for trial in &probe.trials {
            let outcome = &trial.outcome;
            let (il, admins) = match &outcome.token {
                Some(token) => (integrity_name(token.integrity), administrators(token)),
                None => ("-".into(), "-".into()),
            };
            let driver = |state: &str| -> String {
                // "service running, store oem3.inf" as "running/oem3.inf".
                state
                    .replace("service ", "")
                    .replace(", store ", "/")
                    .chars()
                    .take(15)
                    .collect()
            };
            let mut verdict = outcome.verdict();
            if verdict.len() > 160 {
                verdict.truncate(160);
            }
            line(format!(
                "{:>3}  {:<9}  {:<52}  {:<6}  {:<9}  {:<15}  {:<15}  {:<2}  {:<3}  {:<3}  {:<4}  {:<5}  {:>5.1}  {}",
                trial.n,
                trial.phase,
                trial.candidate.describe(&probe.held).chars().take(52).collect::<String>(),
                il,
                admins,
                driver(&outcome.driver_before),
                driver(&outcome.driver_after),
                if outcome.tun_up && outcome.started { "up" } else { "-" },
                check(&outcome.tcp),
                check(&outcome.dns),
                yes(outcome.adapter_gone),
                yes(outcome.route_back),
                outcome.secs,
                verdict
            ));
        }
        line(String::new());
        let names = |candidate: &Option<Candidate>| -> String {
            candidate.as_ref().map_or("none".to_owned(), |candidate| {
                candidate.keep.join(" ") + &candidate.narrowings()
            })
        };
        let dangerous: Vec<String> = [&findings.install, &findings.steady, &findings.install_needs]
            .into_iter()
            .flatten()
            .flat_map(|candidate| candidate.keep.iter())
            .filter(|name| is_never(name))
            .fold(Vec::new(), |mut all, name| {
                if !all.contains(name) {
                    all.push(name.clone());
                }
                all
            });
        let results = [
            ("system_privileges", probe.held.join(" ")),
            ("install_works_with", names(&findings.install)),
            ("steady_minimal", names(&findings.steady)),
            (
                "steady_minimal_complete",
                yes(findings.steady_complete).to_owned(),
            ),
            ("integrity_high", found(findings.high).to_owned()),
            (
                "administrators_deny_only",
                found(findings.deny_admins).to_owned(),
            ),
            ("both_narrowings", found(findings.both).to_owned()),
            ("strictest", names(&findings.strictest)),
            (
                "reinstall_with_strictest",
                found(findings.reinstall.as_ref().map(|(_, works)| *works)).to_owned(),
            ),
            ("install_needs", names(&findings.install_needs)),
            (
                "shipped_install",
                found(findings.shipped_install).to_owned(),
            ),
            ("shipped_steady", found(findings.shipped_steady).to_owned()),
            ("shipped_plan", found(findings.shipped()).to_owned()),
            (
                "dangerous_needed",
                if dangerous.is_empty() {
                    "none".to_owned()
                } else {
                    dangerous.join(" ")
                },
            ),
            ("driver_at_end", driver_state(&probe.system_root)),
        ];
        for (key, value) in &results {
            line(format!("{key}: {value}"));
        }
        line(match (findings.shipped(), dangerous.is_empty()) {
            (Some(true), true) => "regression check: held. The shipped plan installs wintun's \
                                   driver and carries traffic, and no privilege TUN needed is \
                                   one NEVER_FOR_SING_BOX names."
                .to_owned(),
            (shipped, _) => format!(
                "REGRESSION: the shipped plan {}; privileges NEVER_FOR_SING_BOX names that TUN \
                 needed: {}.",
                match shipped {
                    Some(true) => "works",
                    Some(false) => "FAILS",
                    None => "was not fully tried",
                },
                if dangerous.is_empty() {
                    "none".to_owned()
                } else {
                    dangerous.join(" ")
                }
            ),
        });
        if !dangerous.is_empty() {
            line(format!(
                "WARNING: TUN worked only with privileges NEVER_FOR_SING_BOX names: {}. Not \
                 for the allowlist without a decision in ADR 0006.",
                dangerous.join(" ")
            ));
        }

        say!("");
        say!("######## summary");
        for text in out.lines() {
            say(text);
        }
        let result: String = results
            .iter()
            .map(|(key, value)| format!("{key}={value}\n"))
            .collect();
        write_atomic(&probe.work.join("summary.txt"), &out)
            .and_then(|()| write_atomic(&probe.work.join("result.txt"), &result))
            .map_err(|error| format!("writing the summary: {error}"))
    }
}
