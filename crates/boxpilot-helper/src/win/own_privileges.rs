//! The helper's own token (ADR 0006, "Defense in depth"). When it starts,
//! before it verifies a file, opens its pipe or serves anyone, the helper
//! removes from its own process token every privilege
//! `tokenplan::HELPER_TOKEN` doesn't name: `AdjustTokenPrivileges` with
//! `SE_PRIVILEGE_REMOVED`, as WireGuard's tunnel service does
//! (`DropAllPrivileges`). Removed, not disabled: nothing in the process can
//! enable one again. Then it reads its token back and refuses to run
//! (`exit::PRIVILEGES_REFUSED`) if anything beyond the plan is left, or the
//! token can't be read.
//!
//! So the helper runs with at most these whatever the SCM gave it: no
//! required-privilege list (as the MSI installs it), or one an
//! administrator set with `sc.exe privs`, however wide. The console seam
//! drops the same way, which changes nothing that matters for a standard
//! user.

use super::sys::io_error;
use super::token::Token;
use crate::tokenplan::{self, TokenPlan};
use std::io;
use windows::Win32::Foundation::{GetLastError, ERROR_NOT_ALL_ASSIGNED};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_REMOVED, TOKEN_PRIVILEGES,
};

/// Remove from this process's token every privilege `plan` doesn't name,
/// and check it holds nothing beyond `plan` afterwards. Returns the
/// privileges it keeps.
pub(crate) fn keep_only(plan: &TokenPlan<'_>) -> io::Result<Vec<String>> {
    let token = Token::of_process_to_adjust()?;
    let held = token.privileges()?;
    let names: Vec<String> = held
        .iter()
        .map(|privilege| privilege.name.clone())
        .collect();
    let remove = tokenplan::privileges_to_delete(&names, plan.privileges);
    // One at a time, each request a whole TOKEN_PRIVILEGES, so no
    // variable-length buffer is built by hand.
    for privilege in held
        .iter()
        .filter(|privilege| remove.contains(&privilege.name.as_str()))
    {
        let request = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: privilege.luid,
                Attributes: SE_PRIVILEGE_REMOVED,
            }],
        };
        // SAFETY: the token is open with TOKEN_ADJUST_PRIVILEGES; `request`
        // is a whole TOKEN_PRIVILEGES with the one entry it counts; no
        // previous state is asked for.
        unsafe { AdjustTokenPrivileges(token.handle(), false, Some(&request), 0, None, None) }
            .map_err(io_error)
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("removing {}: {error}", privilege.name),
                )
            })?;
        // It succeeds without doing all it was asked, and says so only in
        // the last error.
        // SAFETY: GetLastError takes nothing and reads this thread's value.
        if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
            return Err(io::Error::other(format!(
                "removing {}: not all privileges were removed",
                privilege.name
            )));
        }
    }
    let after = token
        .observed()
        .map_err(|error| io::Error::new(error.kind(), format!("reading it back: {error}")))?;
    let excess = tokenplan::excess(&after, plan);
    if !excess.is_empty() {
        return Err(io::Error::other(format!(
            "its token still holds {}",
            excess.join(", ")
        )));
    }
    Ok(after.privileges.into_iter().map(|(name, _)| name).collect())
}
