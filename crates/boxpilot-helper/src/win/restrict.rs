//! sing-box's token (ADR 0006, "Defense in depth"): a restricted copy of
//! the helper's own primary token, made by `CreateRestrictedToken` as
//! `spawnplan::TokenPlan` says, and checked before anything runs under it.
//!
//! - Built from the helper's *process* token, never a thread's: a thread
//!   that impersonates a pipe client holds the client's token, and the
//!   connection threads do that briefly.
//! - Every privilege the plan doesn't name is deleted
//!   (`PrivilegesToDelete`), so sing-box can't enable it again; disabled
//!   but present would not be enough.
//! - Groups the plan names become deny-only (`SidsToDisable`), and the
//!   integrity level is lowered to the plan's cap. No restricting SIDs: the
//!   copy is not a "restricted token" in `IsTokenRestricted`'s sense, so
//!   nothing treats sing-box as sandboxed.
//! - A restricted copy of the caller's own primary token is assignable to
//!   a child without `SeAssignPrimaryTokenPrivilege`, so the same path works
//!   for the service (SYSTEM) and for the console seam (a standard user).
//! - The copy is read back and judged by `spawnplan::excess`. If it holds
//!   anything beyond the plan, or can't be read, sing-box doesn't start.

use super::security::OwnedSid;
use super::sys::{io_error, own};
use super::token::Token;
use crate::spawnplan::{self, integrity, TokenPlan, SE_GROUP_INTEGRITY};
use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{
    CreateRestrictedToken, GetLengthSid, SetTokenInformation, TokenIntegrityLevel,
    CREATE_RESTRICTED_TOKEN_FLAGS, LUID_AND_ATTRIBUTES, SID_AND_ATTRIBUTES, TOKEN_MANDATORY_LABEL,
    TOKEN_PRIVILEGES_ATTRIBUTES,
};

/// The helper's own token, restricted by `plan`, as a primary token for
/// `CreateProcessAsUserW`.
pub(crate) fn restricted_token(plan: &TokenPlan<'_>) -> io::Result<Token> {
    let own_token = Token::of_process_to_restrict().map_err(context("the helper's own token"))?;

    let held = own_token
        .privileges()
        .map_err(context("the helper's own privileges"))?;
    let names: Vec<String> = held
        .iter()
        .map(|privilege| privilege.name.clone())
        .collect();
    let to_delete = spawnplan::privileges_to_delete(&names, plan.privileges);
    let delete: Vec<LUID_AND_ATTRIBUTES> = held
        .iter()
        .filter(|privilege| to_delete.contains(&privilege.name.as_str()))
        .map(|privilege| LUID_AND_ATTRIBUTES {
            Luid: privilege.luid,
            // Ignored by CreateRestrictedToken.
            Attributes: TOKEN_PRIVILEGES_ATTRIBUTES(0),
        })
        .collect();

    let groups = if plan.deny_only.is_empty() {
        Vec::new()
    } else {
        own_token
            .groups()
            .map_err(context("the helper's own groups"))?
    };
    let disable_sids = spawnplan::sids_to_disable(&groups, plan.deny_only)
        .into_iter()
        .map(OwnedSid::from_string)
        .collect::<io::Result<Vec<_>>>()?;
    let disable: Vec<SID_AND_ATTRIBUTES> = disable_sids
        .iter()
        .map(|sid| SID_AND_ATTRIBUTES {
            Sid: sid.psid(),
            // Ignored by CreateRestrictedToken.
            Attributes: 0,
        })
        .collect();

    let mut handle = HANDLE::default();
    // SAFETY: `own_token` is open with TOKEN_DUPLICATE; both arrays (and
    // the SIDs `disable` points at, owned by `disable_sids`) outlive the
    // call; no restricting SIDs; `handle` receives a new token handle,
    // owned below.
    unsafe {
        CreateRestrictedToken(
            own_token.handle(),
            CREATE_RESTRICTED_TOKEN_FLAGS(0),
            non_empty(&disable),
            non_empty(&delete),
            None,
            &mut handle,
        )
    }
    .map_err(io_error)
    .map_err(context("CreateRestrictedToken"))?;
    // SAFETY: CreateRestrictedToken returned a new handle nothing else owns.
    let token = Token::from_owned(unsafe { own(handle) });

    if plan.max_integrity.is_some() {
        let own_level = own_token
            .integrity()
            .map_err(context("the helper's own integrity level"))?;
        if let Some(level) = spawnplan::integrity_to_set(own_level, plan.max_integrity) {
            set_integrity(&token, level).map_err(context("lowering the integrity level"))?;
        }
    }

    let observed = token
        .observed()
        .map_err(context("reading the restricted token back"))?;
    let excess = spawnplan::excess(&observed, plan);
    if !excess.is_empty() {
        return Err(io::Error::other(format!(
            "sing-box's restricted token still holds {}",
            excess.join(", ")
        )));
    }
    Ok(token)
}

/// An error with what failed in front, its kind kept: the probe's table
/// tells a token that couldn't be made from a TUN that didn't come up.
fn context(what: &'static str) -> impl FnOnce(io::Error) -> io::Error {
    move |error| io::Error::new(error.kind(), format!("{what}: {error}"))
}

/// `None` for an empty list, which CreateRestrictedToken takes as "none".
fn non_empty<T>(items: &[T]) -> Option<&[T]> {
    (!items.is_empty()).then_some(items)
}

/// Lower `token`'s integrity level to `level` (a label RID).
fn set_integrity(token: &Token, level: u32) -> io::Result<()> {
    let sid = OwnedSid::from_string(&integrity::label_sid(level))?;
    let label = TOKEN_MANDATORY_LABEL {
        Label: SID_AND_ATTRIBUTES {
            Sid: sid.psid(),
            Attributes: SE_GROUP_INTEGRITY,
        },
    };
    // SAFETY: `sid` is a valid SID, alive for the call.
    let sid_len = unsafe { GetLengthSid(sid.psid()) };
    let len = size_of::<TOKEN_MANDATORY_LABEL>() as u32 + sid_len;
    // SAFETY: `token` is open with TOKEN_ADJUST_DEFAULT (it has the access of
    // the handle it was made from); `label` and the SID it points at outlive
    // the call, and `len` covers both, as TokenIntegrityLevel expects.
    unsafe {
        SetTokenInformation(
            token.handle(),
            TokenIntegrityLevel,
            (&label as *const TOKEN_MANDATORY_LABEL).cast::<c_void>(),
            len,
        )
    }
    .map_err(io_error)
}
