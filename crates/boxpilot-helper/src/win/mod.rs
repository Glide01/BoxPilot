//! The Windows platform layer (ADR 0006, phase 1): the service, the pipe,
//! the caller's token, the trees' ACLs, the helper's own privileges
//! (dropped when it starts), and sing-box's spawn under its restricted
//! token (`probe` exposes that spawn to the CI token probe).
//!
//! This is the only part of the helper with `unsafe` code: Win32 calls,
//! each in a small wrapper with a `SAFETY` comment saying what holds. The
//! judgements those wrappers feed are the pure, tested modules of the crate
//! (`acl`, `authority`, `spawnplan`, `tun`).

#![warn(clippy::undocumented_unsafe_blocks)]

mod adapters;
mod folders;
mod own_privileges;
mod pipe;
pub mod probe;
mod restrict;
mod security;
mod server;
mod service;
mod spawn;
mod supervisor;
mod sys;
mod token;
mod verify;

use crate::acl::{sid, Trusted};
use crate::cli::{self, Mode, USAGE};
use crate::exit;
use crate::paths::Layout;
use std::path::PathBuf;
use windows::Win32::System::LibraryLoader::{
    SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

/// The helper's entry: the service, or the console seam.
pub fn main() -> i32 {
    // Whatever is loaded after start comes from System32 only, never from
    // the application directory or PATH. The helper's own imports were
    // resolved before this; its directory is admin-only regardless.
    // SAFETY: SetDefaultDllDirectories takes a flag and changes only this
    // process's DLL search order.
    if unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) }.is_err() {
        eprintln!("boxpilot-helper: could not restrict the DLL search path");
        return exit::INTERNAL;
    }
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match cli::parse(&args) {
        Ok(Mode::Service) => service::run(),
        Ok(Mode::Console { root, pipe }) => console(root, &pipe),
        Err(message) => {
            eprintln!("boxpilot-helper: {message}\n{USAGE}");
            exit::USAGE
        }
    }
}

/// The test seam: the same loop, as the user running it, against
/// `<root>\helper` (sing-box and `manifest.json`, copied there beforehand)
/// and `<root>\state`. It trusts that user's own files as the service trusts
/// administrators', which lends nothing only because it never runs
/// elevated: it refuses to.
fn console(root: PathBuf, pipe_name: &str) -> i32 {
    crate::log::echo_to_stderr();
    // As the service does; a standard user loses nothing that matters.
    if let Err(error) = own_privileges::keep_only(&crate::tokenplan::HELPER_TOKEN) {
        eprintln!("boxpilot-helper: dropping its own privileges: {error}");
        return exit::PRIVILEGES_REFUSED;
    }
    let token = match token::Token::of_process() {
        Ok(token) => token,
        Err(error) => {
            eprintln!("boxpilot-helper: the process token: {error}");
            return exit::INTERNAL;
        }
    };
    let user = match token.user() {
        Ok(user) => user,
        Err(error) => {
            eprintln!("boxpilot-helper: the process's account: {error}");
            return exit::INTERNAL;
        }
    };
    if token.elevated().unwrap_or(true) || user == sid::SYSTEM {
        eprintln!("boxpilot-helper: --console runs only unelevated: it trusts its user's files");
        return exit::CONSOLE_ELEVATED;
    }
    let root = match std::path::absolute(&root) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("boxpilot-helper: --root {}: {error}", root.display());
            return exit::USAGE;
        }
    };
    let setup = supervisor::Setup {
        layout: Layout::new(root.join("helper"), root.join("state")),
        trusted: Trusted::administrators_and(&user),
        dir_sddl: security::protected_dir_sddl(Some(&user)),
        clean_adapters: false,
        own_exe: None,
    };
    // Never set: Ctrl+C ends the process, and sing-box's job with it.
    let stop = match sys::Event::new() {
        Ok(stop) => stop,
        Err(error) => {
            eprintln!("boxpilot-helper: {error}");
            return exit::INTERNAL;
        }
    };
    server::run(
        setup,
        pipe_name,
        &pipe::console_pipe_sddl(&user),
        &stop,
        || {
            println!("boxpilot-helper: listening on {pipe_name}");
        },
    )
}
