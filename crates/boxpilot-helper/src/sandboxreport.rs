//! Reading what sing-box's sandbox reported (ADR 0006, "Defense in depth";
//! `sandboxplan`): what CI's macOS job collects over its smoke steps,
//! summed up by operation and target, with sing-box's denials sorted into
//! those `sandboxplan::KNOWN_DENIALS` explains and the unexpected ones,
//! which fail the job. `examples/sandbox_report.rs` prints it from `log
//! show`'s output; the reading is here, pure, so it is tested on every OS.
//!
//! The kernel's sandbox writes one line per report to the unified log
//! (process 0, the sandbox kext as its sender), as CI's runner wrote them,
//! with `log show --style compact`'s timestamp, level and process in front:
//!
//! ```text
//! 2026-10-08 17:43:10.173 Df kernel[0:ad30] (Sandbox) Sandbox: sing-box(15588) allow file-read-data /dev/dtracehelper
//! 2026-10-08 17:42:48.134 E  kernel[0:9b86] (Sandbox) Sandbox: mapssyncd(14783) deny(1) file-read-data /Users/x
//! 2026-10-08 17:42:48.134 E  kernel[0:9b86] (Sandbox) 1 duplicate report for Sandbox: mds(53) deny(1) mach-lookup com.apple.x
//! 2026-10-08 17:43:12.548 Df kernel[0:ade4] (Sandbox) System Policy: diskutil(15622) allow file-read-data /dev/rdisk0
//! ```
//!
//! - **Allows** carry no `(1)`; denials do. The last form, another policy
//!   than a process's own profile, has no `Sandbox: ` and is not read.
//! - **Repeats** are folded into a "duplicate report(s)" line, which counts
//!   that many more.
//! - **sing-box's processes** are the PIDs ever reported as `sing-box`,
//!   whatever they are called in other reports: before its exec, the same
//!   PID is `sandbox-exec`. (sing-box may start nothing: its profile denies
//!   `fork`.) Other sandboxed processes' reports are counted apart.
//! - **Targets** are written with the profile's parameters for the helper's
//!   own paths (`${RUN_DIR}/config.json`, [`HelperPaths`]), so one run's
//!   reports read like the next one's, and like the profile.
//! - **Nothing is dropped quietly.** The format is macOS's, undocumented: a
//!   `Sandbox: ` line this can't read is counted and shown.

#![forbid(unsafe_code)]

use crate::paths::{is_run_name, is_uid};
use crate::sandboxplan::{
    KnownDenial, KNOWN_DENIALS, PARAM_RUN_DIR, PARAM_SING_BOX, PARAM_USER_DIR,
};
use boxpilot_protocol::endpoint::macos;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// sing-box's process name, as the kernel reports it.
pub const SING_BOX: &str = "sing-box";

/// Where a report's own text starts.
const MARK: &str = "Sandbox: ";

/// How many unreadable lines a summary keeps to show.
pub const SAMPLE: usize = 20;

/// How many targets of one operation a summary lists; the raw lines have
/// the rest.
pub const MAX_TARGETS: usize = 400;

/// What a summary writes for the state directory beyond the profile's
/// parameters (the profile itself never names it).
const STATE_DIR: &str = "STATE_DIR";

/// One report: `<process>(<pid>) <verdict> <operation> <target>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The process, as the kernel names it.
    pub process: String,
    pub pid: u32,
    /// `allow` or `deny`, its `(n)` left out.
    pub verdict: String,
    /// The sandbox operation, e.g. `file-write-create`, `process-exec*`.
    pub operation: String,
    /// What it was done to: a path, a mach service, an address, a sysctl…;
    /// empty for an operation without one.
    pub target: String,
    /// How many times: 1, or a duplicate-report line's count.
    pub count: u64,
}

/// What one line of `log show`'s output is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// No sandbox report.
    Other,
    Report(Report),
    /// `Sandbox: ` in a form not understood.
    Unreadable,
}

/// Read one line of `log show`'s output.
pub fn parse_line(line: &str) -> Line {
    let Some(at) = line.find(MARK) else {
        return Line::Other;
    };
    match report(&line[at + MARK.len()..]) {
        Some(mut report) => {
            report.count = duplicates(&line[..at]).unwrap_or(1);
            Line::Report(report)
        }
        None => Line::Unreadable,
    }
}

/// `N` of an `N duplicate report(s) for ` in front of a report.
fn duplicates(before: &str) -> Option<u64> {
    let words: Vec<&str> = before.split_whitespace().collect();
    match words.as_slice() {
        [.., count, "duplicate", "report" | "reports", "for"] => count.parse().ok(),
        _ => None,
    }
}

/// `<process>(<pid>) <verdict>[(<n>)] <operation>[ <target>]`.
fn report(text: &str) -> Option<Report> {
    let (process, pid, rest) = process(text)?;
    let (verdict, rest) = rest.split_once(' ')?;
    let verdict = verdict_word(verdict)?;
    let (operation, target) = rest.split_once(' ').unwrap_or((rest, ""));
    let operation = operation.trim_end();
    let plain =
        |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'*');
    if operation.is_empty() || !operation.bytes().all(plain) {
        return None;
    }
    Some(Report {
        process: process.to_owned(),
        pid,
        verdict: verdict.to_owned(),
        operation: operation.to_owned(),
        target: target.trim().to_owned(),
        count: 1,
    })
}

/// `allow` or `deny`, with an optional `(<digits>)` after it.
fn verdict_word(word: &str) -> Option<&str> {
    let (verdict, count) = match word.split_once('(') {
        Some((verdict, count)) => (verdict, Some(count)),
        None => (word, None),
    };
    if let Some(count) = count {
        let digits = count.strip_suffix(')')?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    matches!(verdict, "allow" | "deny").then_some(verdict)
}

/// The process's name and PID, at the first `(<digits>) ` after a
/// non-empty name (a name may hold spaces and parentheses), and the text
/// after them.
fn process(text: &str) -> Option<(&str, u32, &str)> {
    for (open, _) in text.match_indices('(') {
        if open == 0 {
            continue;
        }
        let after = &text[open + 1..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            continue;
        }
        let Some(rest) = after[digits..].strip_prefix(") ") else {
            continue;
        };
        let pid = after[..digits].parse().ok()?;
        return Some((&text[..open], pid, rest));
    }
    None
}

/// Where the data volume shows its directories (`/Library` among them)
/// besides their firmlinks: a report may name either.
const DATA_VOLUME: &str = "/System/Volumes/Data";

/// The helper's own paths, which a report's target is written in terms
/// of, as the profile's parameters: `${RUN_DIR}` for any run directory,
/// `${USER_DIR}` for any account's state, `${STATE_DIR}` for the rest of
/// the state directory, `${SING_BOX}` for sing-box itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperPaths {
    sing_box: String,
    state_dir: String,
}

impl HelperPaths {
    pub fn new(sing_box: &str, state_dir: &str) -> Self {
        Self {
            sing_box: sing_box.to_owned(),
            state_dir: state_dir.to_owned(),
        }
    }

    /// The installed daemon's (`endpoint::macos`).
    pub fn installed_macos() -> Self {
        Self::new(macos::SING_BOX_PATH, macos::STATE_DIR)
    }

    /// `target`, the helper's paths in it written as parameters.
    pub fn normalize(&self, target: &str) -> String {
        for volume in ["", DATA_VOLUME] {
            if strip(target, &format!("{volume}{}", self.sing_box)) == Some("") {
                return format!("${{{PARAM_SING_BOX}}}");
            }
            if let Some(rest) = strip(target, &format!("{volume}{}", self.state_dir)) {
                return in_state(rest);
            }
        }
        target.to_owned()
    }
}

/// `text` without `prefix`, if what is left is empty or a path under it.
fn strip<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(prefix)?;
    (rest.is_empty() || rest.starts_with('/')).then_some(rest)
}

/// A path in the state directory (`rest` after it), as a parameter and
/// what follows.
fn in_state(rest: &str) -> String {
    let named = |dir: &str, name_ok: fn(&str) -> bool, param: &str| {
        let after = rest
            .strip_prefix('/')?
            .strip_prefix(dir)?
            .strip_prefix('/')?;
        let (name, tail) = after.split_at(after.find('/').unwrap_or(after.len()));
        name_ok(name).then(|| format!("${{{param}}}{tail}"))
    };
    named("runs", is_run_name, PARAM_RUN_DIR)
        .or_else(|| named("users", is_uid, PARAM_USER_DIR))
        .unwrap_or_else(|| format!("${{{STATE_DIR}}}{rest}"))
}

/// The PIDs of sing-box's processes among `reports`: every PID reported as
/// `sing-box`.
fn sing_box_pids(reports: &[Report]) -> BTreeSet<u32> {
    reports
        .iter()
        .filter(|report| report.process == SING_BOX)
        .map(|report| report.pid)
        .collect()
}

/// One of sing-box's denials, and the known denial that explains it, if
/// one does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    pub operation: String,
    /// Normalized, as the summary writes it.
    pub target: String,
    pub count: u64,
    pub known: Option<&'static KnownDenial>,
}

/// Everything a `log show` held, summed up.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    /// sing-box's processes by name, with the PIDs each was reported with.
    pub processes: BTreeMap<String, BTreeSet<u32>>,
    /// Their reports, by operation, then by target (normalized) and
    /// verdict: how many.
    pub reports: BTreeMap<String, BTreeMap<(String, String), u64>>,
    /// Other sandboxed processes' reports, by name.
    pub others: BTreeMap<String, u64>,
    /// How many `Sandbox: ` lines weren't understood.
    pub unreadable: u64,
    /// The first [`SAMPLE`] of them.
    pub unreadable_sample: Vec<String>,
}

impl Summary {
    /// Sum up `lines` (`log show`'s output), the helper's paths in the
    /// targets written as `paths` says.
    pub fn of<'a>(lines: impl IntoIterator<Item = &'a str>, paths: &HelperPaths) -> Self {
        let mut summary = Summary::default();
        let mut parsed = Vec::new();
        for line in lines {
            match parse_line(line) {
                Line::Other => {}
                Line::Report(report) => parsed.push(report),
                Line::Unreadable => {
                    summary.unreadable += 1;
                    if summary.unreadable_sample.len() < SAMPLE {
                        summary.unreadable_sample.push(line.trim().to_owned());
                    }
                }
            }
        }
        let ours = sing_box_pids(&parsed);
        for report in parsed {
            if ours.contains(&report.pid) {
                summary
                    .processes
                    .entry(report.process)
                    .or_default()
                    .insert(report.pid);
                *summary
                    .reports
                    .entry(report.operation)
                    .or_default()
                    .entry((paths.normalize(&report.target), report.verdict))
                    .or_default() += report.count;
            } else {
                *summary.others.entry(report.process).or_default() += report.count;
            }
        }
        summary
    }

    /// How many reports sing-box's processes made.
    pub fn total(&self) -> u64 {
        self.reports.values().flat_map(BTreeMap::values).sum()
    }

    /// Every PID reported as one of sing-box's processes.
    pub fn pids(&self) -> BTreeSet<u32> {
        self.processes.values().flatten().copied().collect()
    }

    /// Of `seen`, the PIDs no report of sing-box's names, in order.
    pub fn unreported(&self, seen: &[u32]) -> Vec<u32> {
        let pids = self.pids();
        let mut unreported: Vec<u32> = seen
            .iter()
            .copied()
            .filter(|pid| !pids.contains(pid))
            .collect();
        unreported.sort_unstable();
        unreported.dedup();
        unreported
    }

    /// sing-box's denials, each with the known denial that explains it.
    pub fn denials(&self) -> Vec<Denial> {
        let mut denials = Vec::new();
        for (operation, targets) in &self.reports {
            for ((target, verdict), count) in targets {
                if verdict != "deny" {
                    continue;
                }
                denials.push(Denial {
                    operation: operation.clone(),
                    target: target.clone(),
                    count: *count,
                    known: KNOWN_DENIALS
                        .iter()
                        .find(|known| known.matches(operation, target)),
                });
            }
        }
        denials
    }

    /// sing-box's denials no known denial explains: each fails CI.
    pub fn unexpected_denials(&self) -> Vec<Denial> {
        self.denials()
            .into_iter()
            .filter(|denial| denial.known.is_none())
            .collect()
    }

    /// sing-box's reports by verdict.
    pub fn verdicts(&self) -> BTreeMap<&str, u64> {
        let mut verdicts = BTreeMap::new();
        for targets in self.reports.values() {
            for ((_, verdict), count) in targets {
                *verdicts.entry(verdict.as_str()).or_default() += count;
            }
        }
        verdicts
    }
}

fn list<T: fmt::Display>(items: impl IntoIterator<Item = T>) -> String {
    items
        .into_iter()
        .map(|item| item.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `text`, or "none" for nothing.
fn or_none(text: String) -> String {
    if text.is_empty() {
        "none".to_owned()
    } else {
        text
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let others: u64 = self.others.values().sum();
        writeln!(
            f,
            "{} reports from sing-box and what it ran, {} operations; {} from other \
             sandboxed processes; {} Sandbox lines not understood",
            self.total(),
            self.reports.len(),
            others,
            self.unreadable
        )?;
        let processes = self.processes.iter().map(|(name, pids)| {
            let word = if pids.len() == 1 { "pid" } else { "pids" };
            format!("{name} ({word} {})", list(pids))
        });
        writeln!(f, "processes: {}", or_none(list(processes)))?;
        let verdicts = self
            .verdicts()
            .into_iter()
            .map(|(verdict, count)| format!("{verdict} {count}"));
        writeln!(f, "verdicts: {}", or_none(list(verdicts)))?;

        writeln!(f)?;
        writeln!(f, "---- by operation: reports, distinct targets")?;
        for (operation, targets) in &self.reports {
            let reports: u64 = targets.values().sum();
            writeln!(f, "  {operation:<40} {reports:>8} {:>6}", targets.len())?;
        }

        writeln!(f)?;
        writeln!(
            f,
            "---- by operation and target: reports, verdict, target (the helper's own paths \
             as the profile's parameters)"
        )?;
        for (operation, targets) in &self.reports {
            let reports: u64 = targets.values().sum();
            writeln!(
                f,
                "{operation} ({reports} reports, {} targets)",
                targets.len()
            )?;
            for ((target, verdict), count) in targets.iter().take(MAX_TARGETS) {
                let target = if target.is_empty() {
                    "(no target)"
                } else {
                    target
                };
                writeln!(f, "  {count:>8}  {verdict:<5}  {target}")?;
            }
            if targets.len() > MAX_TARGETS {
                let rest: u64 = targets.values().skip(MAX_TARGETS).sum();
                writeln!(
                    f,
                    "  … and {} more targets, {rest} reports: see the raw lines",
                    targets.len() - MAX_TARGETS
                )?;
            }
        }

        let denials = self.denials();
        writeln!(f)?;
        writeln!(
            f,
            "---- sing-box's denials: {} ({} unexpected)",
            denials.len(),
            denials
                .iter()
                .filter(|denial| denial.known.is_none())
                .count()
        )?;
        for denial in &denials {
            let target = if denial.target.is_empty() {
                "(no target)"
            } else {
                &denial.target
            };
            match denial.known {
                Some(known) => writeln!(
                    f,
                    "  {:>8}  known       {} {target}: {}",
                    denial.count, denial.operation, known.why
                )?,
                None => writeln!(
                    f,
                    "  {:>8}  UNEXPECTED  {} {target}",
                    denial.count, denial.operation
                )?,
            }
        }

        if !self.others.is_empty() {
            writeln!(f)?;
            writeln!(
                f,
                "---- other sandboxed processes' reports, not sing-box's: {others}"
            )?;
            let mut others: Vec<(&String, &u64)> = self.others.iter().collect();
            others.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
            let shown = others
                .iter()
                .take(40)
                .map(|(name, count)| format!("{name} {count}"));
            writeln!(f, "  {}", list(shown))?;
        }
        if self.unreadable > 0 {
            writeln!(f)?;
            writeln!(
                f,
                "---- Sandbox lines not understood: {} (the first {} below)",
                self.unreadable,
                self.unreadable_sample.len()
            )?;
            for line in &self.unreadable_sample {
                writeln!(f, "  {line}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(line: &str) -> Report {
        match parse_line(line) {
            Line::Report(report) => report,
            other => panic!("{line}: {other:?}"),
        }
    }

    /// The forms macOS writes, as `log show --style compact` prints them.
    #[test]
    fn the_kernels_reports_are_read() {
        let deny = report(
            "2026-10-01 11:03:22.177 E  kernel[0:1a2b] (Sandbox) Sandbox: kernelmanagerd(545) \
             deny(1) file-write-create /private/var/db/loadedkextmt.plist.sb-5a00fc77-LNttZF",
        );
        assert_eq!(
            deny,
            Report {
                process: "kernelmanagerd".into(),
                pid: 545,
                verdict: "deny".into(),
                operation: "file-write-create".into(),
                target: "/private/var/db/loadedkextmt.plist.sb-5a00fc77-LNttZF".into(),
                count: 1,
            }
        );
        let allow =
            report("Sandbox: sing-box(4242) allow file-read-data /Library/Application Support/x y");
        assert_eq!(allow.verdict, "allow");
        assert_eq!(allow.target, "/Library/Application Support/x y");
        let repeated = report(
            "default 11:03:22.177637-0700 kernel 1 duplicate report for Sandbox: icdd(2124) \
             allow file-read-data /Library/Image Capture/Devices",
        );
        assert_eq!(repeated.process, "icdd");
        assert_eq!(repeated.count, 1);
        assert_eq!(repeated.target, "/Library/Image Capture/Devices");
        let many = report(
            "12 duplicate reports for Sandbox: sing-box(7) allow(1) mach-lookup com.apple.x",
        );
        assert_eq!((many.count, many.operation.as_str()), (12, "mach-lookup"));
        assert_eq!(many.target, "com.apple.x");
        let spaced = report("Sandbox: Google Chrome He(123) deny(1) mach-lookup com.apple.y");
        assert_eq!(
            (spaced.process.as_str(), spaced.pid),
            ("Google Chrome He", 123)
        );
        let bare = report("Sandbox: sing-box(7) allow process-fork");
        assert_eq!(
            (bare.operation.as_str(), bare.target.as_str()),
            ("process-fork", "")
        );
        let exec = report("Sandbox: sing-box(7) allow process-exec* /usr/sbin/networksetup");
        assert_eq!(exec.operation, "process-exec*");
        let parens = report("Sandbox: odd (name)(8) allow sysctl-read kern.hostname");
        assert_eq!((parens.process.as_str(), parens.pid), ("odd (name)", 8));
    }

    /// Lines exactly as `log show --style compact` printed them on CI's
    /// runner (macOS 14, Apple silicon) under the measuring profile.
    #[test]
    fn the_lines_ci_recorded_are_read() {
        let allow = report(
            "2026-10-08 17:43:10.173 Df kernel[0:ad30] (Sandbox) Sandbox: sing-box(15588) allow \
             file-read-data /dev/dtracehelper",
        );
        assert_eq!(
            allow,
            Report {
                process: "sing-box".into(),
                pid: 15588,
                verdict: "allow".into(),
                operation: "file-read-data".into(),
                target: "/dev/dtracehelper".into(),
                count: 1,
            }
        );
        let exec = report(
            "2026-10-08 17:43:10.173 Df kernel[0:ad30] (Sandbox) Sandbox: sandbox-exec(15588) \
             allow process-exec* /Library/Application Support/BoxPilot Helper/bin/sing-box",
        );
        assert_eq!(
            (
                exec.process.as_str(),
                exec.operation.as_str(),
                exec.target.as_str()
            ),
            (
                "sandbox-exec",
                "process-exec*",
                "/Library/Application Support/BoxPilot Helper/bin/sing-box"
            )
        );
        let ioctl = report(
            "2026-10-08 17:43:10.174 Df kernel[0:ad30] (Sandbox) Sandbox: sing-box(15588) allow \
             file-ioctl path:/dev/dtracehelper ioctl-command:(_IO \"h\" 4)",
        );
        assert_eq!(
            ioctl.target,
            "path:/dev/dtracehelper ioctl-command:(_IO \"h\" 4)"
        );
        let socket = report(
            "2026-10-08 17:43:10.300 Df kernel[0:ad30] (Sandbox) Sandbox: sing-box(15588) allow \
             system-socket domain:32 type:2 protocol:2",
        );
        assert_eq!(socket.target, "domain:32 type:2 protocol:2");
        let bind = report(
            "2026-10-08 17:43:10.301 Df kernel[0:ad30] (Sandbox) Sandbox: sing-box(15588) allow \
             network-bind local:*:49272",
        );
        assert_eq!(bind.target, "local:*:49272");
        let deny = report(
            "2026-10-08 17:42:48.845 E  kernel[0:a4f7] (Sandbox) Sandbox: mapssyncd(14783) \
             deny(1) file-read-data /Users/runner/Library/Containers",
        );
        assert_eq!((deny.verdict.as_str(), deny.count), ("deny", 1));
        let repeated = report(
            "2026-10-08 17:42:48.134 E  kernel[0:9b86] (Sandbox) 1 duplicate report for Sandbox: \
             RemoteManagementAgent(14528) deny(1) mach-lookup com.apple.metadata.mds",
        );
        assert_eq!(
            (
                repeated.process.as_str(),
                repeated.count,
                repeated.target.as_str()
            ),
            ("RemoteManagementAgent", 1, "com.apple.metadata.mds")
        );
        let folded = report(
            "2026-10-08 17:43:10.200 Df kernel[0:ad30] (Sandbox) 3 duplicate reports for \
             Sandbox: sing-box(15588) allow sysctl-read net.routetable.0.0.1.0",
        );
        assert_eq!((folded.count, folded.pid), (3, 15588));
        // Another policy than the process's own profile: not read.
        for other in [
            "2026-10-08 17:43:12.548 Df kernel[0:ade4] (Sandbox) System Policy: diskutil(15622) \
             allow file-read-data /dev/rdisk0",
            "2026-10-08 17:44:44.360 I  kernel[0:c965] (Sandbox) successfully truncated homedirs \
             to 22 bytes",
        ] {
            assert_eq!(parse_line(other), Line::Other, "{other}");
        }
    }

    #[test]
    fn what_isnt_a_report_is_told_apart() {
        for other in [
            "",
            "Timestamp               Ty Process[PID:TID]",
            "kernel[0:1] (Sandbox) something else",
            "Violation:       deny(1) file-read-data /x",
        ] {
            assert_eq!(parse_line(other), Line::Other, "{other}");
        }
        for unreadable in [
            "Sandbox: ",
            "Sandbox: sing-box allow file-read-data /x",
            "Sandbox: sing-box(x) allow file-read-data /x",
            "Sandbox: (7) allow file-read-data /x",
            "Sandbox: sing-box(7) permit file-read-data /x",
            "Sandbox: sing-box(7) deny(x) file-read-data /x",
            "Sandbox: sing-box(7) deny(1)",
            "Sandbox: sing-box(7) allow File-Read /x",
        ] {
            assert_eq!(parse_line(unreadable), Line::Unreadable, "{unreadable}");
        }
    }

    #[test]
    fn the_helpers_paths_read_as_the_profiles_parameters() {
        let paths = HelperPaths::installed_macos();
        let state = macos::STATE_DIR;
        let run = "0123456789abcdef0123456789abcdef";
        for (target, expected) in [
            (
                format!("{state}/runs/{run}/config.json"),
                "${RUN_DIR}/config.json".to_owned(),
            ),
            (format!("{state}/runs/{run}"), "${RUN_DIR}".to_owned()),
            (
                format!("{state}/users/501/cache.db"),
                "${USER_DIR}/cache.db".to_owned(),
            ),
            (
                format!("{state}/users/501/tailscale/x"),
                "${USER_DIR}/tailscale/x".to_owned(),
            ),
            (format!("{state}/owner"), "${STATE_DIR}/owner".to_owned()),
            (state.to_owned(), "${STATE_DIR}".to_owned()),
            (
                format!("{state}/runs/not-a-run/x"),
                "${STATE_DIR}/runs/not-a-run/x".to_owned(),
            ),
            (
                format!("{state}/users/0501"),
                "${STATE_DIR}/users/0501".to_owned(),
            ),
            (macos::SING_BOX_PATH.to_owned(), "${SING_BOX}".to_owned()),
            (
                format!("/System/Volumes/Data{state}/runs/{run}/tmp"),
                "${RUN_DIR}/tmp".to_owned(),
            ),
            (
                format!("/System/Volumes/Data{}", macos::SING_BOX_PATH),
                "${SING_BOX}".to_owned(),
            ),
            // Only the helper's own paths: a neighbour sharing a prefix
            // stays as it is.
            (format!("{state}x/owner"), format!("{state}x/owner")),
            (
                format!("{}x", macos::SING_BOX_PATH),
                format!("{}x", macos::SING_BOX_PATH),
            ),
            (
                "/usr/sbin/networksetup".to_owned(),
                "/usr/sbin/networksetup".to_owned(),
            ),
            ("com.apple.x".to_owned(), "com.apple.x".to_owned()),
        ] {
            assert_eq!(paths.normalize(&target), expected, "{target}");
        }
    }

    /// sing-box's processes are its PIDs, sandbox-exec before its exec
    /// included; another program under the same profile (CI's probe) or its
    /// own isn't sing-box's. Its denials are sorted into known and
    /// unexpected ones.
    #[test]
    fn sing_boxs_reports_and_denials_are_summed_up() {
        let state = macos::STATE_DIR;
        let run = "0123456789abcdef0123456789abcdef";
        let text = format!(
            "\
Timestamp               Ty Process[PID:TID]
2026-10-01 11:00:00.000 Df kernel[0:1] (Sandbox) Sandbox: sandbox-exec(100) allow process-exec* {sing_box}
2026-10-01 11:00:00.001 Df kernel[0:1] (Sandbox) Sandbox: sing-box(100) allow file-read-data {state}/runs/{run}/config.json
2026-10-01 11:00:00.002 Df kernel[0:1] (Sandbox) 3 duplicate reports for Sandbox: sing-box(100) allow file-read-data {state}/runs/{run}/config.json
2026-10-01 11:00:00.003 Df kernel[0:1] (Sandbox) Sandbox: sing-box(100) allow file-write-create {state}/users/501/cache.db
2026-10-01 11:00:00.004 E  kernel[0:1] (Sandbox) Sandbox: sing-box(100) deny(1) process-fork
2026-10-01 11:00:00.005 E  kernel[0:1] (Sandbox) Sandbox: sing-box(100) deny(1) file-read-data /private/etc/master.passwd
2026-10-01 11:00:00.006 E  kernel[0:1] (Sandbox) 1 duplicate report for Sandbox: sing-box(100) deny(1) process-fork
2026-10-01 11:00:00.007 E  kernel[0:1] (Sandbox) Sandbox: sing-box(100) deny(1) file-write-create /Library/Preferences/x.plist
2026-10-01 11:00:00.008 Df kernel[0:1] (Sandbox) Sandbox: sandbox-exec(200) allow process-exec* /private/var/tmp/probe/bin/sandbox-probe
2026-10-01 11:00:00.009 E  kernel[0:1] (Sandbox) Sandbox: sandbox-probe(200) deny(1) file-read-data /Users/runner/x
2026-10-01 11:00:00.010 E  kernel[0:1] (Sandbox) Sandbox: mds(55) deny(1) file-read-data /Users/a/b
2026-10-01 11:00:00.011 Df kernel[0:1] (Sandbox) Sandbox: sing-box(300) allow file-read-data {state}/runs/{run}/config.json
2026-10-01 11:00:00.012 Df kernel[0:1] (Sandbox) Sandbox: sing-box(300) gibberish
2026-10-01 11:00:00.013 Df kernel[0:1] (Sandbox) System Policy: diskutil(15622) allow file-read-data /dev/rdisk0
",
            sing_box = macos::SING_BOX_PATH,
        );
        let summary = Summary::of(text.lines(), &HelperPaths::installed_macos());
        let names: Vec<&str> = summary.processes.keys().map(String::as_str).collect();
        assert_eq!(names, ["sandbox-exec", "sing-box"]);
        assert_eq!(summary.pids(), BTreeSet::from([100, 300]));
        assert_eq!(summary.processes["sandbox-exec"], BTreeSet::from([100]));
        // Every line of 100 and 300 but the gibberish; the duplicates count
        // as many as they say.
        assert_eq!(summary.total(), 1 + 1 + 3 + 1 + 1 + 1 + 1 + 1 + 1);
        assert_eq!(
            summary.reports["file-read-data"]
                [&("${RUN_DIR}/config.json".to_owned(), "allow".to_owned())],
            5
        );
        assert_eq!(
            summary.reports["process-exec*"][&("${SING_BOX}".to_owned(), "allow".to_owned())],
            1
        );
        assert_eq!(
            summary.reports["file-write-create"]
                [&("${USER_DIR}/cache.db".to_owned(), "allow".to_owned())],
            1
        );
        assert_eq!(
            summary.others,
            BTreeMap::from([
                ("mds".to_owned(), 1),
                ("sandbox-exec".to_owned(), 1),
                ("sandbox-probe".to_owned(), 1)
            ])
        );
        assert_eq!(summary.unreadable, 1);
        assert!(summary.unreadable_sample[0].ends_with("sing-box(300) gibberish"));
        assert_eq!(
            summary.verdicts(),
            BTreeMap::from([("allow", 7), ("deny", 4)])
        );
        assert_eq!(summary.unreported(&[300, 100, 999, 999, 7]), [7, 999]);

        let denials = summary.denials();
        assert_eq!(denials.len(), 3, "{denials:?}");
        let fork = denials
            .iter()
            .find(|denial| denial.operation == "process-fork")
            .unwrap();
        assert_eq!((fork.count, fork.target.as_str()), (2, ""));
        assert!(fork.known.is_some());
        let unexpected = summary.unexpected_denials();
        assert_eq!(unexpected.len(), 1);
        assert_eq!(
            (
                unexpected[0].operation.as_str(),
                unexpected[0].target.as_str()
            ),
            ("file-write-create", "/Library/Preferences/x.plist")
        );

        let shown = summary.to_string();
        for expected in [
            "11 reports from sing-box and what it ran, 4 operations; 3 from other sandboxed \
             processes; 1 Sandbox lines not understood",
            "processes: sandbox-exec (pid 100), sing-box (pids 100, 300)",
            "verdicts: allow 7, deny 4",
            "file-read-data (6 reports, 2 targets)",
            "         5  allow  ${RUN_DIR}/config.json",
            "         1  allow  ${SING_BOX}",
            "         2  deny   (no target)",
            "---- sing-box's denials: 3 (1 unexpected)",
            "         1  UNEXPECTED  file-write-create /Library/Preferences/x.plist",
            "         2  known       process-fork (no target): sing-tun flushes DNS",
            "  mds 1",
            "sing-box(300) gibberish",
        ] {
            assert!(shown.contains(expected), "{expected:?} in\n{shown}");
        }
    }

    #[test]
    fn nothing_of_sing_boxs_is_an_empty_summary() {
        let summary = Summary::of(
            ["Sandbox: mds(55) deny(1) file-read-data /x", "unrelated"],
            &HelperPaths::installed_macos(),
        );
        assert_eq!(summary.total(), 0);
        assert!(summary.processes.is_empty());
        assert_eq!(summary.others["mds"], 1);
        assert_eq!(summary.unreported(&[1]), [1]);
        let shown = summary.to_string();
        assert!(shown.starts_with("0 reports from sing-box"), "{shown}");
        assert!(
            shown.contains("\nprocesses: none\nverdicts: none\n"),
            "{shown}"
        );
    }

    #[test]
    fn a_long_list_of_targets_is_cut_with_its_count() {
        let lines: Vec<String> = (0..MAX_TARGETS + 3)
            .map(|n| format!("Sandbox: sing-box(1) allow file-read-metadata /x/{n:04}"))
            .collect();
        let summary = Summary::of(
            lines.iter().map(String::as_str),
            &HelperPaths::installed_macos(),
        );
        assert_eq!(summary.total(), MAX_TARGETS as u64 + 3);
        assert!(summary
            .to_string()
            .contains("… and 3 more targets, 3 reports: see the raw lines"));
    }
}
