//! Who may start and stop sing-box through the macOS helper (ADR 0006 rule
//! 4), decided from the peer's uid, which the kernel gives the helper for
//! each connection (`LOCAL_PEERCRED`), and the owner record, as pure data so
//! it is tested on every OS. The PID is never used.
//!
//! - **The owner** may start and stop: the account an administrator
//!   authorized through the install prompt, whose uid the install script
//!   wrote, root-owned, into the state directory
//!   (`endpoint::macos::OWNER_FILE`). Another account takes over with an
//!   administrator prompt of its own; the last one authorized holds it, as
//!   with ADR 0003's grant on Linux.
//! - **Root** may too. A process running as root is already beyond any
//!   boundary the helper draws: it could rewrite the owner record, or run
//!   sing-box itself. Refusing it would protect nothing, and letting it in
//!   lets an administrator's `sudo` tooling (and CI's smoke test) drive the
//!   helper without being the owner. What a root caller gets is still only
//!   what the owner gets: a start on a config the policy checked.
//! - **Everyone else** is read-only: `hello` and `status`, under the same
//!   read-only connection cap as on Windows (`conn::ReadOnlySlots`).
//! - **No owner record, or one the helper can't read or parse, means nobody
//!   may start, root included**: the install is broken, and the helper
//!   fails closed rather than guess who should be trusted instead.
//!   Reinstalling writes it again.

#![forbid(unsafe_code)]

use crate::paths::is_uid;
use boxpilot_protocol::Authority;
use std::fmt;

/// A larger owner record is refused unread: a real one is a uid and a
/// newline.
pub const MAX_OWNER_BYTES: usize = 64;

/// Why an owner record was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerError {
    TooLarge(usize),
    /// Not a uid as `paths::is_uid` takes it, with at most one newline
    /// after it.
    Malformed,
}

impl fmt::Display for OwnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OwnerError::TooLarge(bytes) => write!(
                f,
                "the owner record is {bytes} bytes, over the {MAX_OWNER_BYTES}-byte limit"
            ),
            OwnerError::Malformed => f.write_str("the owner record is not a uid"),
        }
    }
}

impl std::error::Error for OwnerError {}

/// The owner's uid from the owner record's bytes: decimal, as the install
/// script writes it (`printf '%s\n'`), with at most one trailing newline.
pub fn parse_owner(bytes: &[u8]) -> Result<u32, OwnerError> {
    if bytes.len() > MAX_OWNER_BYTES {
        return Err(OwnerError::TooLarge(bytes.len()));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| OwnerError::Malformed)?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    if !is_uid(text) {
        return Err(OwnerError::Malformed);
    }
    text.parse().map_err(|_| OwnerError::Malformed)
}

/// The authority of a caller whose uid is `peer`, when the owner record
/// names `owner` (`None`: missing or unreadable).
pub fn authority(peer: u32, owner: Option<u32>) -> Authority {
    match owner {
        Some(owner) if peer == owner || peer == 0 => Authority::MayStart,
        _ => Authority::ReadOnly,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_owner_record_is_a_uid_and_a_newline() {
        assert_eq!(parse_owner(b"501\n"), Ok(501));
        assert_eq!(parse_owner(b"501"), Ok(501));
        assert_eq!(parse_owner(b"0\n"), Ok(0));
        for bad in [
            &b""[..],
            b"\n",
            b"501\n\n",
            b"\n501",
            b" 501",
            b"501 \n",
            b"0501\n",
            b"+501",
            b"-1",
            b"4294967295",
            b"5o1",
            b"501\r\n",
            b"\xff",
            b"501\0",
        ] {
            assert_eq!(
                parse_owner(bad),
                Err(OwnerError::Malformed),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        assert_eq!(
            parse_owner(&[b'1'; MAX_OWNER_BYTES + 1]),
            Err(OwnerError::TooLarge(MAX_OWNER_BYTES + 1))
        );
    }

    #[test]
    fn the_owner_and_root_may_start() {
        assert_eq!(authority(501, Some(501)), Authority::MayStart);
        assert_eq!(authority(0, Some(501)), Authority::MayStart);
    }

    #[test]
    fn everyone_else_is_read_only() {
        for peer in [502, 1, 500, 4294967294] {
            assert_eq!(authority(peer, Some(501)), Authority::ReadOnly, "{peer}");
        }
    }

    /// A broken install: nobody may start, root included.
    #[test]
    fn without_an_owner_nobody_may_start() {
        for peer in [0, 501] {
            assert_eq!(authority(peer, None), Authority::ReadOnly, "{peer}");
        }
    }
}
