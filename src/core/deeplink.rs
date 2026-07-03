//! Import-URI parsing for the `sing-box://` URL scheme (and the BoxPilot
//! alias `boxpilot://`).
//!
//! The canonical form, shared by the official sing-box clients
//! (SFA/SFI/SFM) and most third-party GUIs:
//!
//! ```text
//! sing-box://import-remote-profile?url=<percent-encoded URL>#<percent-encoded name>
//! ```
//!
//! Parsing is hand-rolled (percent-decode + query split) to avoid pulling in
//! a URL crate for one fixed shape. Pure logic, no platform code — the
//! Windows plumbing that delivers these strings lives in
//! `core/single_instance.rs`.

/// Schemes we accept, lowercase, including the `://` separator.
pub const SCHEMES: [&str; 2] = ["sing-box://", "boxpilot://"];

/// Cheap pre-filter for argv: is this argument a deep link at all? Used to
/// decide whether to hand it to the single-instance forwarder; full
/// validation happens later in [`parse_import_uri`].
pub fn is_deeplink(arg: &str) -> bool {
    let lower = arg.trim().to_ascii_lowercase();
    SCHEMES.iter().any(|scheme| lower.starts_with(scheme))
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportRequest {
    /// The subscription URL to fetch (http/https, already decoded).
    pub url: String,
    /// Display name from the URI fragment (or `name` query param), if any.
    pub name: Option<String>,
}

pub fn parse_import_uri(uri: &str) -> Result<ImportRequest, String> {
    let trimmed = uri.trim();
    let lower = trimmed.to_ascii_lowercase();
    let rest = SCHEMES
        .iter()
        .find_map(|scheme| {
            lower
                .starts_with(scheme)
                // Schemes are pure ASCII, so byte slicing is safe.
                .then(|| &trimmed[scheme.len()..])
        })
        .ok_or_else(|| "unsupported URL scheme".to_string())?;

    let (body, fragment) = match rest.split_once('#') {
        Some((body, fragment)) => (body, Some(fragment)),
        None => (rest, None),
    };
    let (action, query) = match body.split_once('?') {
        Some((action, query)) => (action, query),
        None => (body, ""),
    };
    let action = action.trim_end_matches('/');
    if !action.eq_ignore_ascii_case("import-remote-profile") {
        return Err(format!("unsupported action \"{}\"", action));
    }

    let mut url = None;
    let mut name_param = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key.eq_ignore_ascii_case("url") {
            url = Some(percent_decode(value));
        } else if key.eq_ignore_ascii_case("name") {
            name_param = Some(percent_decode(value));
        }
    }
    let url = url
        .filter(|u| !u.is_empty())
        .ok_or_else(|| "missing url parameter".to_string())?;
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("profile URL must be http:// or https://".to_string());
    }

    // The spec puts the name in the fragment; some generators use a `name`
    // query param instead. Fragment wins when both are present.
    let name = fragment
        .map(percent_decode)
        .or(name_param)
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());

    Ok(ImportRequest { url, name })
}

/// Fallback profile name when the URI carries none: the subscription host.
pub fn derive_profile_name(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        "Imported".to_string()
    } else {
        host.to_string()
    }
}

/// Decode %XX escapes; malformed escapes are kept literally rather than
/// rejected (browsers and chat apps mangle links often enough that lenient
/// beats strict here). `+` is NOT decoded to space — these values are
/// percent-encoded URLs/names, not HTML form data.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_sing_box_uri_with_name_fragment() {
        let req = parse_import_uri(
            "sing-box://import-remote-profile?url=https%3A%2F%2Fexample.com%2Fsub%3Ftoken%3Dabc#My%20Provider",
        )
        .unwrap();
        assert_eq!(req.url, "https://example.com/sub?token=abc");
        assert_eq!(req.name.as_deref(), Some("My Provider"));
    }

    #[test]
    fn boxpilot_alias_scheme_is_accepted() {
        let req =
            parse_import_uri("boxpilot://import-remote-profile?url=https%3A%2F%2Fa.example%2Fs")
                .unwrap();
        assert_eq!(req.url, "https://a.example/s");
        assert_eq!(req.name, None);
    }

    #[test]
    fn scheme_and_action_match_case_insensitively() {
        let req = parse_import_uri(
            "SING-BOX://Import-Remote-Profile?URL=https%3A%2F%2Fa.example%2Fs",
        )
        .unwrap();
        assert_eq!(req.url, "https://a.example/s");
    }

    #[test]
    fn trailing_slash_after_action_is_tolerated() {
        let req = parse_import_uri(
            "sing-box://import-remote-profile/?url=https%3A%2F%2Fa.example%2Fs",
        )
        .unwrap();
        assert_eq!(req.url, "https://a.example/s");
    }

    #[test]
    fn name_query_param_is_a_fallback_but_fragment_wins() {
        let via_param = parse_import_uri(
            "sing-box://import-remote-profile?url=https%3A%2F%2Fa.example%2Fs&name=Param",
        )
        .unwrap();
        assert_eq!(via_param.name.as_deref(), Some("Param"));

        let both = parse_import_uri(
            "sing-box://import-remote-profile?url=https%3A%2F%2Fa.example%2Fs&name=Param#Frag",
        )
        .unwrap();
        assert_eq!(both.name.as_deref(), Some("Frag"));
    }

    #[test]
    fn utf8_names_decode() {
        let req = parse_import_uri(
            "sing-box://import-remote-profile?url=https%3A%2F%2Fa.example%2Fs#%E8%8A%82%E7%82%B9",
        )
        .unwrap();
        assert_eq!(req.name.as_deref(), Some("节点"));
    }

    #[test]
    fn rejects_unknown_scheme_action_and_missing_url() {
        assert!(parse_import_uri("https://example.com/sub").is_err());
        assert!(parse_import_uri("sing-box://do-something?url=https%3A%2F%2Fa.b").is_err());
        assert!(parse_import_uri("sing-box://import-remote-profile").is_err());
        assert!(parse_import_uri("sing-box://import-remote-profile?url=").is_err());
    }

    #[test]
    fn rejects_non_http_profile_url() {
        // file:// or other schemes inside the url param must not be fetched.
        assert!(
            parse_import_uri("sing-box://import-remote-profile?url=file%3A%2F%2F%2Fetc%2Fhosts")
                .is_err()
        );
    }

    #[test]
    fn is_deeplink_filters_argv() {
        assert!(is_deeplink("sing-box://import-remote-profile?url=x"));
        assert!(is_deeplink("  BoxPilot://anything"));
        assert!(!is_deeplink("C:\\Users\\me\\config.json"));
        assert!(!is_deeplink("--flag"));
    }

    #[test]
    fn malformed_percent_escapes_are_kept_literally() {
        assert_eq!(percent_decode("100%25"), "100%");
        assert_eq!(percent_decode("bad%2"), "bad%2");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    #[test]
    fn derive_name_extracts_host() {
        assert_eq!(
            derive_profile_name("https://sub.example.com:8443/path?x=1"),
            "sub.example.com"
        );
        assert_eq!(
            derive_profile_name("https://user:pass@host.example/sub"),
            "host.example"
        );
        assert_eq!(derive_profile_name("nonsense"), "nonsense");
        assert_eq!(derive_profile_name("https://"), "Imported");
    }
}
