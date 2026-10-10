//! The POSIX platform layer (ADR 0006, the macOS phase): what the macOS
//! helper does through POSIX calls, kept apart from what only macOS has
//! (`mac`: launchd's socket, `networksetup`, the system log), so it builds
//! and is tested on Linux too, where the helper binary itself still only
//! says it is unsupported:
//!
//! - [`transport`]: one connection, a Unix socket with deadlines;
//! - [`peer`]: who connected, by the uid the kernel recorded;
//! - [`server`]: the accept loop, its connection limits and its idle exit;
//! - [`verify`]: the helper's trees, by owner and mode (`modes`);
//! - [`child`]: sing-box's `posix_spawn`, and a stop and reap that never
//!   signal a reused PID;
//! - [`supervisor`]: the `Supervisor` over all of that;
//! - [`signals`]: launchd's SIGTERM, taken by a thread.
//!
//! Its `unsafe` code is the libc calls `std` doesn't make, each in a small
//! wrapper with a `SAFETY` comment saying what holds. The judgements they
//! feed are the crate's pure modules (`modes`, `owner`, `cleanup`,
//! `spawnplan`).

#![warn(clippy::undocumented_unsafe_blocks)]

pub mod child;
pub mod peer;
pub mod server;
pub mod signals;
pub mod supervisor;
pub mod transport;
pub mod verify;
