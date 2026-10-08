//! Hooks for the token probe, `examples/token_probe.rs`: the CI step that
//! measures, on a real Windows machine, which token sing-box needs for TUN
//! (ADR 0006, "Defense in depth"). They start sing-box down the helper's
//! own path (`spawn::spawn`: the restricted token, the job, the handle
//! list, the mitigations, the environment built from nothing), with a plan
//! the probe chooses, and read tokens and adapters back.
//!
//! The helper never calls them: it starts sing-box with
//! `spawnplan::SING_BOX_TOKEN` alone, a constant, and nothing that reaches
//! the helper (its pipe, its files, its environment) reaches these. They
//! are public only because an example can't see the crate's private items.

use super::adapters;
use super::folders;
use super::spawn::{self, Job, Launch};
use super::sys::{raw, wait_handle};
use super::token::Token;
use crate::spawnplan::{ObservedToken, TokenPlan};
use std::fs::File;
use std::io;
use std::os::windows::io::OwnedHandle;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A sing-box the probe started, in its own job.
pub struct Run {
    process: OwnedHandle,
    job: Job,
    pid: u32,
}

/// The read ends of a started sing-box's stdout and stderr.
pub struct Output {
    pub stdout: File,
    pub stderr: File,
}

/// Start `program` exactly as the helper starts sing-box, but under
/// `plan`. Fails, as the helper's spawn does, when the restricted token
/// can't be made or holds more than `plan`.
pub fn start(
    program: &Path,
    args: Vec<String>,
    cwd: &Path,
    environment: Vec<(String, String)>,
    plan: &TokenPlan<'_>,
) -> io::Result<(Run, Output)> {
    let child = spawn::spawn(&Launch {
        program,
        args,
        cwd,
        environment,
        token: plan,
    })?;
    Ok((
        Run {
            process: child.process,
            job: child.job,
            pid: child.pid,
        },
        Output {
            stdout: child.stdout,
            stderr: child.stderr,
        },
    ))
}

impl Run {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The token sing-box runs with, read from its process.
    pub fn token(&self) -> io::Result<ObservedToken> {
        Token::of_process_handle(raw(&self.process))?.observed()
    }

    /// End it as the helper does: the whole job.
    pub fn stop(&self) {
        self.job.terminate();
    }

    /// Its exit code once it has exited, waiting at most `timeout`; `None`
    /// if it still runs.
    pub fn wait(&self, timeout: Duration) -> io::Result<Option<u32>> {
        if wait_handle(&self.process, Some(Instant::now() + timeout))? {
            spawn::exit_code(&self.process).map(Some)
        } else {
            Ok(None)
        }
    }
}

/// This process's own token, and its account's SID.
pub fn own_token() -> io::Result<(String, ObservedToken)> {
    let token = Token::of_process()?;
    Ok((token.user()?, token.observed()?))
}

/// Every installed sing-tun adapter, and whether it is present.
pub fn sing_tun_adapters() -> Vec<(String, bool)> {
    adapters::sing_tun_adapters()
}

/// Remove the sing-tun adapters that are no longer present, as the helper
/// does after each run; how many it removed.
pub fn remove_stale_sing_tun_adapters() -> u32 {
    adapters::remove_sing_tun_adapters()
}

/// The Windows directory, as the helper reads it for sing-box's
/// environment.
pub fn windows_dir() -> io::Result<String> {
    folders::windows_dir()
}

/// `%ProgramFiles%`, as the helper finds its own tree.
pub fn program_files() -> io::Result<PathBuf> {
    folders::program_files()
}
