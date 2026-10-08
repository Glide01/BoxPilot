//! `sandbox_report`: what sing-box's sandbox profile reported (ADR 0006,
//! "Defense in depth"; `sandboxplan`), from the kernel's sandbox reports as
//! `log show` printed them, summed up by `sandboxreport`: by operation and
//! target, and sing-box's denials, each one `sandboxplan::KNOWN_DENIALS`
//! explains or unexpected.
//!
//! CI's macOS job runs it through `packaging/macos/helper-smoke.sh
//! sandbox-reports`, over the reports of every smoke step that ran
//! sing-box: the TUN runs, the GUI client's, the helper killed under it,
//! the system proxy. It is the profile's regression check: an unexpected
//! denial is something sing-box tried that the profile doesn't allow. It
//! is a measuring tool: nothing installs it, and it reads only the files it
//! is given.
//!
//! Exit code 0 when sing-box's reports were found and none of its denials
//! is unexpected; 3 when one is; 1 when no report names sing-box (the
//! collection is broken, or sing-box ran unsandboxed: the enforced profile
//! always reports dyld's `/dev/dtracehelper`), and then a raw sample of the
//! file is printed instead, so the next look isn't blind; 2 for a bad
//! command line or a file it can't read.

use boxpilot_helper::sandboxplan::STATUS;
use boxpilot_helper::sandboxreport::{HelperPaths, Summary};
use std::fs;
use std::io;

const USAGE: &str = "\
usage: sandbox_report <log show output> [--pids <file>]

  <log show output>  the unified log's sandbox reports, as `log show --style compact` prints them
  --pids <file>      the sing-box PIDs the smoke steps saw, one per line: the summary says which
                     no report names";

const FOUND: i32 = 0;
const NONE_FOUND: i32 = 1;
const USAGE_ERROR: i32 = 2;
const UNEXPECTED_DENIALS: i32 = 3;

/// How many lines a raw sample shows.
const RAW_SAMPLE: usize = 40;

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (log, pids) = match args.as_slice() {
        [log] => (log, None),
        [log, flag, pids] if flag == "--pids" => (log, Some(pids)),
        _ => {
            eprintln!("{USAGE}");
            return USAGE_ERROR;
        }
    };
    let text = match fs::read(log) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => {
            eprintln!("sandbox_report: {log}: {error}");
            return USAGE_ERROR;
        }
    };
    let seen: Vec<u32> = match pids.map(fs::read_to_string) {
        None => Vec::new(),
        Some(Ok(text)) => text
            .split_whitespace()
            .filter_map(|word| word.parse().ok())
            .collect(),
        Some(Err(error)) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Some(Err(error)) => {
            eprintln!("sandbox_report: the PID file: {error}");
            return USAGE_ERROR;
        }
    };

    let summary = Summary::of(text.lines(), &HelperPaths::installed_macos());
    println!("==== sing-box's sandbox reports (the profile is {STATUS})");
    print!("{summary}");
    if !seen.is_empty() {
        let unreported = summary.unreported(&seen);
        println!();
        println!(
            "---- the sing-box PIDs the smoke steps saw: {}; named in no report: {}",
            seen.len(),
            if unreported.is_empty() {
                "none".to_owned()
            } else {
                // The kernel folds a report another process just made into
                // its duplicates, so a PID may go unnamed; every one would
                // mean the profile wasn't applied.
                unreported
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
    }
    if summary.total() > 0 {
        let unexpected = summary.unexpected_denials();
        if unexpected.is_empty() {
            return FOUND;
        }
        println!();
        println!(
            "==== {} of sing-box's denials no known denial explains (sandboxplan::KNOWN_DENIALS):",
            unexpected.len()
        );
        for denial in &unexpected {
            println!(
                "  {} {} ({} reports)",
                denial.operation, denial.target, denial.count
            );
        }
        return UNEXPECTED_DENIALS;
    }

    println!();
    println!("==== no report names sing-box: a raw sample of {log}, so the next look isn't blind");
    let sample = |what: &str, keep: &dyn Fn(&str) -> bool| {
        let lines: Vec<&str> = text.lines().filter(|line| keep(line)).collect();
        println!(
            "---- {what}: {} lines (the first {})",
            lines.len(),
            lines.len().min(RAW_SAMPLE)
        );
        for line in lines.iter().take(RAW_SAMPLE) {
            println!("  {line}");
        }
        !lines.is_empty()
    };
    let sandbox = sample("lines that mention a sandbox", &|line| {
        line.to_ascii_lowercase().contains("sandbox")
    });
    let sing_box = sample("lines that mention sing-box", &|line| {
        line.contains("sing-box")
    });
    if !sandbox && !sing_box {
        sample("every line", &|_| true);
    }
    NONE_FOUND
}
