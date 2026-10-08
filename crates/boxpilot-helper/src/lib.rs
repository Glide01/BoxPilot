//! BoxPilot's privileged helper (ADR 0006): a service that runs sing-box as
//! SYSTEM for TUN mode, on behalf of a GUI that stays unprivileged, on a
//! config the helper has checked itself.
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
//! The judgements the Windows layer makes are pure functions here too, so
//! they are tested on every OS: [`acl`] (who may write where the helper
//! reads from), [`authority`] (who may start), [`spawnplan`] (sing-box's
//! command line and environment), [`tokenplan`] (what sing-box's token and
//! the helper's own keep) and [`tun`] (which adapters to remove).
//!
//! The platform layer lives in `win`, under `cfg(windows)`: the service,
//! the pipe, the caller's token, directory ACLs, and the spawn. It is the
//! only module with `unsafe` code, each block in a small wrapper with its
//! `SAFETY` comment.
//!
//! [`Authority`]: boxpilot_protocol::Authority

#![deny(unsafe_code)]

pub mod acl;
pub mod authority;
pub mod cli;
pub mod conn;
pub mod exit;
pub mod helper;
pub mod lines;
pub mod log;
pub mod manifest;
pub mod outbox;
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

#[cfg(test)]
mod testing;
