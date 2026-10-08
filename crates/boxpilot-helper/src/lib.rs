//! BoxPilot's privileged helper (ADR 0006): a service that runs sing-box as
//! SYSTEM on Windows, and a launchd daemon that runs it as root on macOS,
//! for TUN mode, on behalf of a GUI that stays unprivileged, on a config the
//! helper has checked itself.
//!
//! Nothing the GUI sends is trusted. A connection's [`Authority`] comes from
//! the OS; its requests go through `boxpilot_protocol`'s session; a `start`'s
//! config goes through `boxpilot_policy` and runs only as the helper rewrote
//! it ([`runcfg`]); the sing-box binary is the one the install manifest
//! names, by hash ([`manifest`]).
//!
//! The modules here are the helper's cross-platform core, tested on every
//! OS:
//!
//! - [`conn`] drives one connection over any [`transport::Transport`]: the
//!   frame decoder, the session, deadlines, the memory budget, and the
//!   bounded [`outbox`] that sing-box's [`lines`] go out through;
//! - [`helper`] is the machine-wide state behind every connection: one
//!   sing-box at a time, started through a platform [`helper::Supervisor`];
//! - [`runcfg`] turns a `start` into the config sing-box runs, and
//!   [`rundir`] writes it into a fresh run directory.
//!
//! The judgements the platform layers make are pure functions here too, so
//! they are tested on every OS:
//!
//! - Windows: [`acl`] (who may write where the helper reads from),
//!   [`authority`] (who may start), [`tokenplan`] (what sing-box's token
//!   and the helper's own keep) and [`tun`] (which adapters to remove);
//! - macOS: [`modes`] (who may write where the helper reads from, by owner
//!   and mode), [`owner`] (who may start), [`cleanup`] (what to undo after
//!   a run, or a crash) and [`launchd`] (the daemon's plist);
//! - both: [`spawnplan`] (sing-box's command line and environment).
//!
//! The platform layers are the only modules with `unsafe` code, each block
//! in a small wrapper with its `SAFETY` comment:
//!
//! - `win`, under `cfg(windows)`: the service, the pipe, the caller's
//!   token, directory ACLs, and the spawn;
//! - `posix`, on macOS and Linux: the Unix socket, the peer's uid, the
//!   accept loop, the trees' modes, `posix_spawn`, and the supervisor. It
//!   is built and tested on Linux, where the helper binary itself is still
//!   unsupported;
//! - `mac`, under `cfg(target_os = "macos")`: launchd's socket, the
//!   `networksetup` and DNS cleanup, the system log, and the daemon's
//!   entry.
//!
//! [`Authority`]: boxpilot_protocol::Authority

#![deny(unsafe_code)]

pub mod acl;
pub mod authority;
pub mod cleanup;
pub mod cli;
pub mod conn;
pub mod exit;
pub mod helper;
pub mod launchd;
pub mod lines;
pub mod log;
pub mod manifest;
pub mod modes;
pub mod outbox;
pub mod owner;
pub mod paths;
pub mod runcfg;
pub mod rundir;
pub mod spawnplan;
pub mod tokenplan;
pub mod transport;
pub mod tun;

#[cfg(windows)]
#[allow(unsafe_code)]
pub mod win;

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(unsafe_code)]
pub mod posix;

#[cfg(test)]
mod testing;
