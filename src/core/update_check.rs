//! BoxPilot release update check against GitHub releases.
//!
//! Pure parts — response parsing, version comparison, the notify rule — plus
//! one blocking fetch, which callers run off the UI thread
//! (`AppState::check_for_updates`). Only the BoxPilot version is checked
//! here; the bundled sing-box version ships with each BoxPilot release.

use crate::i18n::s;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde::Deserialize;
use std::cmp::Ordering;
use std::time::Duration;

/// GitHub's "latest release" endpoint for this repository. It never returns
/// drafts or prereleases, so a prerelease tag is never offered.
pub const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/Glide01/BoxPilot/releases/latest";

/// Where "Download" goes when a response carries no usable release page.
pub const RELEASES_PAGE: &str = "https://github.com/Glide01/BoxPilot/releases/latest";

/// The whole request — connect, headers, body — gives up after this.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The first automatic check waits this long after start, so it never
/// competes with startup work (window, sing-box autostart, subscriptions).
pub const FIRST_CHECK_DELAY: Duration = Duration::from_secs(20);

/// Automatic checks run at most this often.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The running BoxPilot version (the Cargo package version; release tags are
/// `v` + this).
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// One published BoxPilot release, as the update check reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    /// The tag without its `v` prefix, e.g. `1.14.0` — the form shown in the
    /// UI and stored as the skipped version.
    pub version: String,
    /// The tag as published, e.g. `v1.14.0`.
    pub tag: String,
    /// The release page ("Download" opens it). Always `https://`.
    pub url: String,
    /// The release notes (Markdown), empty when there are none.
    pub notes: String,
}

/// The fields of GitHub's release object this check reads; everything else
/// in the response (assets, author, …) is ignored.
#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// Parse a `GET /repos/{owner}/{repo}/releases/latest` response body.
/// Drafts, prereleases and tags that aren't a plain `[v]X.Y.Z` version are
/// refused — none of them is something to offer as an update.
pub fn parse_latest_release(json: &str) -> Result<ReleaseInfo, String> {
    let release: ApiRelease =
        serde_json::from_str(json).map_err(|_| s().updates.unexpected_response.to_string())?;
    if release.draft || release.prerelease {
        return Err(s().updates.prerelease.to_string());
    }
    let tag = release.tag_name.trim().to_string();
    if parse_version(&tag).is_none() {
        return Err((s().updates.bad_tag)(&tag));
    }
    let url = release
        .html_url
        .map(|url| url.trim().to_string())
        .filter(|url| url.starts_with("https://"))
        .unwrap_or_else(|| RELEASES_PAGE.to_string());
    Ok(ReleaseInfo {
        version: normalize_version(&tag).to_string(),
        tag,
        url,
        notes: release.body.unwrap_or_default().trim().to_string(),
    })
}

/// `v1.2.3` / `V1.2.3` / ` 1.2.3 ` → `1.2.3`.
pub fn normalize_version(version: &str) -> &str {
    let version = version.trim();
    version.strip_prefix(['v', 'V']).unwrap_or(version)
}

/// The numeric parts of a release version, or `None` for anything else —
/// including prerelease forms like `1.14.0-rc.1`. Build metadata
/// (`+build.5`) is ignored, as semver does.
fn parse_version(version: &str) -> Option<Vec<u64>> {
    let version = normalize_version(version);
    let version = version.split_once('+').map_or(version, |(core, _)| core);
    if version.is_empty() {
        return None;
    }
    version
        .split('.')
        .map(|part| {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                None
            } else {
                part.parse().ok()
            }
        })
        .collect()
}

/// Compare numerically, part by part; missing parts count as 0, so `1.14`
/// equals `1.14.0`.
fn compare_parts(a: &[u64], b: &[u64]) -> Ordering {
    let len = a.len().max(b.len());
    (0..len)
        .map(|i| {
            let x = a.get(i).copied().unwrap_or(0);
            let y = b.get(i).copied().unwrap_or(0);
            x.cmp(&y)
        })
        .find(|ord| ord.is_ne())
        .unwrap_or(Ordering::Equal)
}

/// Whether `candidate` is a later release than `current` (`1.13.10` >
/// `1.13.9`; a `v` prefix on either side is fine). Unparseable versions are
/// never newer.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => compare_parts(&candidate, &current).is_gt(),
        _ => false,
    }
}

/// Whether two version strings name the same release (`v1.14` = `1.14.0`).
pub fn same_version(a: &str, b: &str) -> bool {
    match (parse_version(a), parse_version(b)) {
        (Some(a), Some(b)) => compare_parts(&a, &b).is_eq(),
        _ => normalize_version(a) == normalize_version(b),
    }
}

/// Whether finding `latest` should tell the user (one toast): it's newer
/// than the running version, the user hasn't skipped it, and this session
/// hasn't already told them about it.
pub fn should_notify(
    latest: &str,
    current: &str,
    skipped: Option<&str>,
    already_notified: Option<&str>,
) -> bool {
    is_newer(latest, current)
        && !skipped.is_some_and(|skipped| same_version(skipped, latest))
        && !already_notified.is_some_and(|notified| same_version(notified, latest))
}

/// The endpoint the check asks. Debug builds honour `BOXPILOT_UPDATE_URL`
/// (for exercising the UI against a local fixture server); release builds
/// always ask GitHub.
pub fn latest_release_api() -> String {
    #[cfg(debug_assertions)]
    if let Some(url) = std::env::var("BOXPILOT_UPDATE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
    {
        return url.trim().to_string();
    }
    LATEST_RELEASE_API.to_string()
}

/// The proxy the check goes through: sing-box's local mixed inbound while
/// it runs in Proxy mode. In TUN mode the system route captures the request
/// anyway, and with sing-box stopped there is nothing to go through.
pub fn check_proxy(proxy_mode: bool, sing_box_running: bool, proxy_port: u16) -> Option<String> {
    (proxy_mode && sing_box_running).then(|| format!("http://127.0.0.1:{proxy_port}"))
}

/// Ask GitHub for the latest release. Blocking (up to [`FETCH_TIMEOUT`]) —
/// never call it on the UI thread. `proxy` (e.g. `http://127.0.0.1:7890`)
/// routes the request through sing-box; `None` connects directly, ignoring
/// any system/environment proxy (which may point at a sing-box that isn't
/// running).
pub fn fetch_latest(proxy: Option<&str>) -> Result<ReleaseInfo, String> {
    fetch_latest_from(&latest_release_api(), proxy)
}

fn fetch_latest_from(url: &str, proxy: Option<&str>) -> Result<ReleaseInfo, String> {
    let builder = Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent(format!("BoxPilot/{CURRENT_VERSION}"));
    let builder = match proxy {
        Some(proxy) => builder
            .proxy(reqwest::Proxy::all(proxy).map_err(|_| s().updates.invalid_proxy.to_string())?),
        None => builder.no_proxy(),
    };
    let client = builder
        .build()
        .map_err(|_| s().updates.client_setup.to_string())?;
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .map_err(|e| request_error(&e))?;
    let status = response.status();
    if !status.is_success() {
        return Err(status_error(status));
    }
    let body = response.text().map_err(|e| request_error(&e))?;
    parse_latest_release(&body)
}

/// A short reason for the status line — never the full error chain.
fn request_error(err: &reqwest::Error) -> String {
    let t = &s().updates;
    if err.is_timeout() {
        t.timed_out.to_string()
    } else if err.is_connect() {
        t.cannot_connect.to_string()
    } else if err.is_body() || err.is_decode() {
        t.interrupted.to_string()
    } else {
        t.network_error.to_string()
    }
}

fn status_error(status: StatusCode) -> String {
    match status.as_u16() {
        404 => s().updates.no_release.to_string(),
        403 | 429 => s().updates.rate_limited.to_string(),
        code => (s().updates.http_status)(code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Trimmed from a real `releases/latest` response: same shape, nested
    /// objects included.
    const FIXTURE: &str = r###"{
      "url": "https://api.github.com/repos/Glide01/BoxPilot/releases/123456789",
      "assets_url": "https://api.github.com/repos/Glide01/BoxPilot/releases/123456789/assets",
      "html_url": "https://github.com/Glide01/BoxPilot/releases/tag/v1.14.0",
      "id": 123456789,
      "author": {
        "login": "github-actions[bot]",
        "id": 41898282,
        "type": "Bot",
        "site_admin": false
      },
      "node_id": "RE_kwDOExample",
      "tag_name": "v1.14.0",
      "target_commitish": "main",
      "name": "BoxPilot v1.14.0",
      "draft": false,
      "immutable": false,
      "prerelease": false,
      "created_at": "2026-09-30T08:00:00Z",
      "published_at": "2026-09-30T08:10:00Z",
      "assets": [
        {
          "name": "BoxPilot-1.14.0-x86_64.AppImage",
          "content_type": "application/octet-stream",
          "size": 41234567,
          "browser_download_url": "https://github.com/Glide01/BoxPilot/releases/download/v1.14.0/BoxPilot-1.14.0-x86_64.AppImage"
        },
        {
          "name": "BoxPilot-1.14.0-x64.msi",
          "content_type": "application/x-msi",
          "size": 23456789,
          "browser_download_url": "https://github.com/Glide01/BoxPilot/releases/download/v1.14.0/BoxPilot-1.14.0-x64.msi"
        }
      ],
      "tarball_url": "https://api.github.com/repos/Glide01/BoxPilot/tarball/v1.14.0",
      "zipball_url": "https://api.github.com/repos/Glide01/BoxPilot/zipball/v1.14.0",
      "body": "## What's new\r\n\r\n- Traffic chart on Home\r\n- Update check\r\n"
    }"###;

    #[test]
    fn parses_the_github_release_fixture() {
        let info = parse_latest_release(FIXTURE).unwrap();
        assert_eq!(
            info,
            ReleaseInfo {
                version: "1.14.0".to_string(),
                tag: "v1.14.0".to_string(),
                url: "https://github.com/Glide01/BoxPilot/releases/tag/v1.14.0".to_string(),
                notes: "## What's new\r\n\r\n- Traffic chart on Home\r\n- Update check".to_string(),
            }
        );
    }

    #[test]
    fn parse_tolerates_missing_or_odd_optional_fields() {
        let info =
            parse_latest_release(r#"{"tag_name":"1.14.0","html_url":null,"body":null}"#).unwrap();
        assert_eq!(info.version, "1.14.0");
        assert_eq!(info.tag, "1.14.0");
        assert_eq!(info.url, RELEASES_PAGE);
        assert_eq!(info.notes, "");

        // Only https pages are ever opened.
        let info =
            parse_latest_release(r#"{"tag_name":"v2.0.0","html_url":"javascript:alert(1)"}"#)
                .unwrap();
        assert_eq!(info.url, RELEASES_PAGE);
    }

    #[test]
    fn parse_refuses_what_is_not_an_update() {
        for json in [
            "",
            "not json",
            "{}",
            r#"{"message":"Not Found","documentation_url":"https://docs.github.com"}"#,
            r#"{"tag_name":"v1.14.0","prerelease":true}"#,
            r#"{"tag_name":"v1.14.0","draft":true}"#,
            r#"{"tag_name":"v1.14.0-rc.1"}"#,
            r#"{"tag_name":"nightly"}"#,
            r#"{"tag_name":""}"#,
        ] {
            assert!(parse_latest_release(json).is_err(), "{json:?} parsed");
        }
    }

    #[test]
    fn is_newer_compares_numerically() {
        let cases = [
            ("1.13.10", "1.13.9", true),
            ("1.13.9", "1.13.10", false),
            ("v1.14.0", "1.13.5", true),
            ("1.14.0", "v1.13.5", true),
            ("V2.0.0", "1.99.99", true),
            ("1.13.5", "1.13.5", false),
            ("v1.13.5", "1.13.5", false),
            ("1.14", "1.14.0", false),
            ("1.14.0.1", "1.14.0", true),
            ("1.14.1", "1.14", true),
            ("2.0.0", "10.0.0", false),
            ("10.0.0", "9.9.9", true),
            ("1.14.0+build.7", "1.13.0", true),
            // Prereleases and garbage are never offered.
            ("1.14.0-rc.1", "1.13.0", false),
            ("nightly", "1.13.0", false),
            ("", "1.13.0", false),
            ("1..0", "0.1", false),
            ("1.14.0", "garbage", false),
        ];
        for (candidate, current, expected) in cases {
            assert_eq!(
                is_newer(candidate, current),
                expected,
                "is_newer({candidate:?}, {current:?})"
            );
        }
    }

    #[test]
    fn same_version_ignores_prefix_and_trailing_zeros() {
        assert!(same_version("v1.14.0", "1.14.0"));
        assert!(same_version("1.14", "1.14.0"));
        assert!(!same_version("1.14.1", "1.14.0"));
        assert!(same_version("weird", "weird"));
    }

    #[test]
    fn should_notify_table() {
        let cases = [
            // (latest, current, skipped, notified, expected)
            ("1.14.0", "1.13.5", None, None, true),
            ("1.13.5", "1.13.5", None, None, false),
            ("1.13.4", "1.13.5", None, None, false),
            ("1.14.0", "1.13.5", Some("1.14.0"), None, false),
            ("1.14.0", "1.13.5", Some("v1.14.0"), None, false),
            ("1.14.0", "1.13.5", None, Some("1.14.0"), false),
            // A release newer than the skipped / already-told one tells again.
            ("1.14.1", "1.13.5", Some("1.14.0"), None, true),
            ("1.14.1", "1.13.5", None, Some("1.14.0"), true),
        ];
        for (latest, current, skipped, notified, expected) in cases {
            assert_eq!(
                should_notify(latest, current, skipped, notified),
                expected,
                "should_notify({latest:?}, {current:?}, {skipped:?}, {notified:?})"
            );
        }
    }

    #[test]
    fn current_version_parses() {
        assert!(parse_version(CURRENT_VERSION).is_some());
    }

    /// Serve one HTTP response on a loopback port; returns the URL and the
    /// request the client sent.
    fn one_shot_server(response: String) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/releases/latest", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (url, handle)
    }

    fn http_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn fetch_reads_the_release_and_identifies_itself() {
        let (url, server) = one_shot_server(http_response("200 OK", FIXTURE));
        let info = fetch_latest_from(&url, None).unwrap();
        assert_eq!(info.version, "1.14.0");
        let request = server.join().unwrap().to_ascii_lowercase();
        assert!(request
            .contains(&format!("user-agent: boxpilot/{CURRENT_VERSION}").to_ascii_lowercase()));
        assert!(request.contains("accept: application/vnd.github+json"));
    }

    #[test]
    fn fetch_maps_http_errors_to_short_reasons() {
        let (url, server) = one_shot_server(http_response(
            "403 Forbidden",
            r#"{"message":"API rate limit exceeded"}"#,
        ));
        let err = fetch_latest_from(&url, None).unwrap_err();
        server.join().unwrap();
        assert_eq!(err, "GitHub rate limit reached, try again later");

        let (url, server) = one_shot_server(http_response("404 Not Found", "{}"));
        let err = fetch_latest_from(&url, None).unwrap_err();
        server.join().unwrap();
        assert_eq!(err, "no release published yet");
    }

    #[test]
    fn proxy_only_while_running_in_proxy_mode() {
        assert_eq!(
            check_proxy(true, true, 18200).as_deref(),
            Some("http://127.0.0.1:18200")
        );
        assert_eq!(check_proxy(true, false, 18200), None);
        assert_eq!(check_proxy(false, true, 18200), None);
        assert_eq!(check_proxy(false, false, 18200), None);
    }

    #[test]
    fn fetch_goes_through_the_given_proxy() {
        // The "proxy" answers the forwarded request itself; the host in the
        // URL never resolves, so only a proxied request can succeed.
        let (proxy, server) = one_shot_server(http_response("200 OK", FIXTURE));
        let proxy = proxy.trim_end_matches("/releases/latest").to_string();
        let info =
            fetch_latest_from("http://api.github.invalid/releases/latest", Some(&proxy)).unwrap();
        assert_eq!(info.version, "1.14.0");
        let request = server.join().unwrap();
        assert!(
            request.starts_with("GET http://api.github.invalid/releases/latest HTTP/1.1"),
            "{request}"
        );
    }

    #[test]
    fn fetch_reports_an_unreachable_server_briefly() {
        // Bind then drop: nothing listens on this port any more.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = fetch_latest_from(&format!("http://127.0.0.1:{port}/"), None).unwrap_err();
        assert_eq!(err, "couldn't connect");
    }
}
