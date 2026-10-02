//! OpenVPN client endpoints (`endpoints[]` of type `openvpn-client`):
//! `SubscribeOpenVPNStatus`, `SubmitOpenVPNChallengeResponse`,
//! `CancelOpenVPNChallenge`, plus the pure rules for answering a challenge.
//!
//! Where challenges come from upstream (sing-openvpn `challenge.go`,
//! `client_supervisor.go`, `control_directives.go`):
//! - `credentials`: the config sets `static_challenge` but no
//!   `username`/`password` — asked once before connecting: username,
//!   password, and the answer to `secret_message`.
//! - `secret`: a static challenge with configured credentials, a dynamic
//!   (`CRV1`) challenge after the server rejected the first login, or a
//!   server `CR_TEXT` that requires a response — one answer to `message`.
//! - `message`: a server `CR_TEXT` that requires no response; informational.
//! - `open-url`: the server wants a web sign-in (`OPEN_URL`/`WEB_AUTH`); the
//!   server itself completes it once the user is done in the browser.
//!
//! The last two can't be answered, only cancelled; they also carry the
//! server's `deadline`. Cancelling any challenge fails the endpoint for good
//! (`error` until sing-box restarts).

use super::openconnect::VpnState;
use super::transport::{ApiError, IDLE_STREAM_READ_TIMEOUT};
use super::{pb, SingBoxApi};
use std::time::Duration;

/// A server-pushed answer is written to the control channel before the call
/// returns — quick, but more than a plain read.
const CHALLENGE_ACTION_TIMEOUT: Duration = Duration::from_secs(15);

impl SingBoxApi {
    /// Stream `SubscribeOpenVPNStatus`: on subscribe, one update with every
    /// OpenVPN client endpoint (an empty list when the config has none),
    /// then a full update whenever any of them changes. Idle otherwise:
    /// `TimedOut` after `IDLE_STREAM_READ_TIMEOUT`; re-subscribe.
    pub fn stream_openvpn_status(
        &self,
        mut on_update: impl FnMut(Vec<OpenVpnEndpointStatus>) -> bool,
    ) -> Result<(), ApiError> {
        self.stream(
            "SubscribeOpenVPNStatus",
            &(),
            IDLE_STREAM_READ_TIMEOUT,
            |update: pb::OpenVpnStatusUpdate| {
                on_update(
                    update
                        .endpoints
                        .into_iter()
                        .map(OpenVpnEndpointStatus::from_proto)
                        .collect(),
                )
            },
        )
    }

    /// Answer a `credentials` or `secret` challenge (see `openvpn_answer`).
    /// Errors: `NOT_FOUND` (no such endpoint), `INVALID_ARGUMENT` (not an
    /// OpenVPN client), `UNKNOWN` when the challenge is no longer pending or
    /// can't be answered (`message`, `open-url`).
    pub fn submit_openvpn_challenge(
        &self,
        endpoint_tag: &str,
        challenge_id: &str,
        answer: &OpenVpnAnswer,
    ) -> Result<(), ApiError> {
        let request = pb::OpenVpnChallengeSubmission {
            endpoint_tag: endpoint_tag.to_string(),
            challenge_id: challenge_id.to_string(),
            username: answer.username.clone(),
            password: answer.password.clone(),
            secret: answer.secret.clone(),
        };
        self.unary_with_timeout(
            "SubmitOpenVPNChallengeResponse",
            &request,
            CHALLENGE_ACTION_TIMEOUT,
        )
    }

    /// `CancelOpenVPNChallenge` — refuse the challenge. The endpoint gives
    /// up ("challenge canceled") and stays in `error` until sing-box
    /// restarts.
    pub fn cancel_openvpn_challenge(
        &self,
        endpoint_tag: &str,
        challenge_id: &str,
    ) -> Result<(), ApiError> {
        let request = pb::OpenVpnChallengeCancel {
            endpoint_tag: endpoint_tag.to_string(),
            challenge_id: challenge_id.to_string(),
        };
        self.unary_with_timeout("CancelOpenVPNChallenge", &request, CHALLENGE_ACTION_TIMEOUT)
    }
}

/// One OpenVPN client endpoint's status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenVpnEndpointStatus {
    pub endpoint_tag: String,
    pub state: VpnState,
    /// `state` as display text, in English.
    pub state_text: String,
    /// Why the endpoint gave up; empty unless `state` is `Error`.
    pub error: String,
    /// Set exactly while `state` is `AuthPending`.
    pub challenge: Option<OpenVpnChallenge>,
    /// Set exactly while `state` is `Connected`.
    pub tunnel: Option<OpenVpnTunnel>,
}

impl OpenVpnEndpointStatus {
    fn from_proto(status: pb::OpenVpnEndpointStatus) -> Self {
        Self {
            endpoint_tag: status.endpoint_tag,
            state: VpnState::from_proto(&status.state),
            state_text: status.state_text,
            error: status.error,
            challenge: status.challenge.map(OpenVpnChallenge::from_proto),
            tunnel: status.tunnel_info.map(OpenVpnTunnel::from_proto),
        }
    }
}

/// The established tunnel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenVpnTunnel {
    /// The remote in use (`host:port`).
    pub server: String,
    /// `udp` or `tcp`.
    pub network: String,
    /// Negotiated data cipher.
    pub cipher: String,
    /// Assigned addresses as prefixes.
    pub ipv4: Vec<String>,
    pub ipv6: Vec<String>,
    pub dns: Vec<String>,
    pub mtu: u32,
    /// Unix seconds.
    pub connected_since: Option<i64>,
}

impl OpenVpnTunnel {
    fn from_proto(info: pb::OpenVpnTunnelInfo) -> Self {
        Self {
            server: info.server,
            network: info.network,
            cipher: info.cipher,
            ipv4: info.ipv4,
            ipv6: info.ipv6,
            dns: info.dns,
            mtu: info.mtu,
            connected_since: (info.connected_since > 0).then_some(info.connected_since),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenVpnChallengeKind {
    Credentials,
    Secret,
    Message,
    OpenUrl,
    /// A kind newer than this client; can only be cancelled.
    Other(String),
}

impl OpenVpnChallengeKind {
    fn from_proto(kind: &str) -> Self {
        match kind {
            "credentials" => OpenVpnChallengeKind::Credentials,
            "secret" => OpenVpnChallengeKind::Secret,
            "message" => OpenVpnChallengeKind::Message,
            "open-url" => OpenVpnChallengeKind::OpenUrl,
            other => OpenVpnChallengeKind::Other(other.to_string()),
        }
    }
}

/// A pending challenge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenVpnChallenge {
    /// Answer or cancel by this id.
    pub id: String,
    pub kind: OpenVpnChallengeKind,
    /// `Credentials`: the prefill. Otherwise the account being challenged,
    /// for display.
    pub username: String,
    /// `Secret` / `Message`: the server's (or config's) challenge text.
    pub message: String,
    /// `OpenUrl`: the sign-in page.
    pub url: String,
    /// `Credentials`: the static challenge text; empty = no third field.
    pub secret_message: String,
    /// The answer may be shown while typed (an OTP, not a password).
    pub echo: bool,
    /// Why the previous attempt failed; empty on the first try.
    pub previous_error: String,
    /// Unix seconds by which the server gives up waiting.
    pub deadline: Option<i64>,
}

impl OpenVpnChallenge {
    fn from_proto(challenge: pb::OpenVpnChallenge) -> Self {
        Self {
            id: challenge.id,
            kind: OpenVpnChallengeKind::from_proto(&challenge.kind),
            username: challenge.username,
            message: challenge.message,
            url: challenge.url,
            secret_message: challenge.secret_message,
            echo: challenge.echo,
            previous_error: challenge.previous_error,
            deadline: (challenge.deadline > 0).then_some(challenge.deadline),
        }
    }

    /// Which inputs to show and whether there is anything to submit.
    pub fn prompt(&self) -> OpenVpnPrompt {
        let secret = |label: &str| {
            Some(OpenVpnSecretPrompt {
                label: label.trim().to_string(),
                echo: self.echo,
            })
        };
        match self.kind {
            OpenVpnChallengeKind::Credentials => OpenVpnPrompt {
                credentials: true,
                secret: if self.secret_message.trim().is_empty() {
                    None
                } else {
                    secret(&self.secret_message)
                },
                open_url: None,
            },
            OpenVpnChallengeKind::Secret => OpenVpnPrompt {
                credentials: false,
                secret: secret(&self.message),
                open_url: None,
            },
            OpenVpnChallengeKind::OpenUrl => OpenVpnPrompt {
                credentials: false,
                secret: None,
                open_url: (!self.url.is_empty()).then(|| self.url.clone()),
            },
            OpenVpnChallengeKind::Message | OpenVpnChallengeKind::Other(_) => {
                OpenVpnPrompt::default()
            }
        }
    }
}

/// What a challenge asks the user for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenVpnPrompt {
    /// Ask for username (prefilled) and password.
    pub credentials: bool,
    /// Ask for one challenge answer.
    pub secret: Option<OpenVpnSecretPrompt>,
    /// Send the user to this page; the server finishes on its own.
    pub open_url: Option<String>,
}

impl OpenVpnPrompt {
    /// Whether `submit_openvpn_challenge` applies; otherwise the challenge
    /// can only be waited out or cancelled.
    pub fn answerable(&self) -> bool {
        self.credentials || self.secret.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenVpnSecretPrompt {
    /// The challenge text to show above the input.
    pub label: String,
    /// Show the answer as typed; mask it otherwise.
    pub echo: bool,
}

/// A challenge answer. Fields a challenge kind doesn't use stay empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenVpnAnswer {
    pub username: String,
    pub password: String,
    pub secret: String,
}

/// Build the answer for `challenge` from what the user typed, sending only
/// the fields its prompt asked for. Rejects an unanswerable challenge, an
/// empty username, and an empty challenge answer (the server requires one).
pub fn openvpn_answer(
    challenge: &OpenVpnChallenge,
    username: &str,
    password: &str,
    secret: &str,
) -> Result<OpenVpnAnswer, String> {
    let prompt = challenge.prompt();
    if !prompt.answerable() {
        return Err("This request can't be answered here.".to_string());
    }
    let mut answer = OpenVpnAnswer::default();
    if prompt.credentials {
        if username.trim().is_empty() {
            return Err("Enter a username.".to_string());
        }
        answer.username = username.to_string();
        answer.password = password.to_string();
    }
    if prompt.secret.is_some() {
        if secret.is_empty() {
            return Err("Enter a response to the challenge.".to_string());
        }
        answer.secret = secret.to_string();
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn challenge(kind: &str) -> OpenVpnChallenge {
        OpenVpnChallenge::from_proto(pb::OpenVpnChallenge {
            id: "ab12".into(),
            kind: kind.into(),
            username: "alice".into(),
            message: "Enter OTP".into(),
            url: "https://sso.example.com/login".into(),
            secret_message: "Token code".into(),
            echo: true,
            previous_error: String::new(),
            deadline: 0,
        })
    }

    #[test]
    fn status_maps_every_field() {
        let status = OpenVpnEndpointStatus::from_proto(pb::OpenVpnEndpointStatus {
            endpoint_tag: "office".into(),
            state: "auth-pending".into(),
            state_text: "Waiting for authentication".into(),
            challenge: Some(pb::OpenVpnChallenge {
                id: "ab12".into(),
                kind: "secret".into(),
                username: "alice".into(),
                message: "Enter OTP".into(),
                url: String::new(),
                secret_message: String::new(),
                echo: false,
                previous_error: "AUTH_FAILED".into(),
                deadline: 1_800_000_060,
            }),
            error: String::new(),
            tunnel_info: Some(pb::OpenVpnTunnelInfo {
                server: "vpn.example.com:1194".into(),
                network: "udp".into(),
                ipv4: vec!["10.8.0.6/24".into()],
                ipv6: Vec::new(),
                dns: vec!["10.8.0.1".into()],
                mtu: 1500,
                connected_since: 0,
                cipher: "AES-256-GCM".into(),
            }),
        });
        assert_eq!(status.endpoint_tag, "office");
        assert_eq!(status.state, VpnState::AuthPending);
        assert_eq!(status.state_text, "Waiting for authentication");
        assert_eq!(
            status.challenge,
            Some(OpenVpnChallenge {
                id: "ab12".into(),
                kind: OpenVpnChallengeKind::Secret,
                username: "alice".into(),
                message: "Enter OTP".into(),
                url: String::new(),
                secret_message: String::new(),
                echo: false,
                previous_error: "AUTH_FAILED".into(),
                deadline: Some(1_800_000_060),
            })
        );
        assert_eq!(
            status.tunnel,
            Some(OpenVpnTunnel {
                server: "vpn.example.com:1194".into(),
                network: "udp".into(),
                cipher: "AES-256-GCM".into(),
                ipv4: vec!["10.8.0.6/24".into()],
                ipv6: Vec::new(),
                dns: vec!["10.8.0.1".into()],
                mtu: 1500,
                connected_since: None,
            })
        );
    }

    #[test]
    fn prompts_follow_the_challenge_kind() {
        let credentials = challenge("credentials").prompt();
        assert!(credentials.credentials && credentials.answerable());
        assert_eq!(
            credentials.secret,
            Some(OpenVpnSecretPrompt {
                label: "Token code".into(),
                echo: true
            })
        );

        let mut plain = challenge("credentials");
        plain.secret_message.clear();
        assert_eq!(plain.prompt().secret, None, "no static challenge text");

        let secret = challenge("secret").prompt();
        assert!(!secret.credentials && secret.answerable());
        assert_eq!(secret.secret.unwrap().label, "Enter OTP");

        let open_url = challenge("open-url").prompt();
        assert!(!open_url.answerable());
        assert_eq!(
            open_url.open_url.as_deref(),
            Some("https://sso.example.com/login")
        );

        assert!(!challenge("message").prompt().answerable());
        let future = challenge("qr-code");
        assert_eq!(future.kind, OpenVpnChallengeKind::Other("qr-code".into()));
        assert_eq!(future.prompt(), OpenVpnPrompt::default());
    }

    #[test]
    fn answers_carry_only_what_the_prompt_asked() {
        assert_eq!(
            openvpn_answer(&challenge("credentials"), "alice", "pw", "123456"),
            Ok(OpenVpnAnswer {
                username: "alice".into(),
                password: "pw".into(),
                secret: "123456".into(),
            })
        );
        assert_eq!(
            openvpn_answer(&challenge("secret"), "ignored", "ignored", "123456"),
            Ok(OpenVpnAnswer {
                secret: "123456".into(),
                ..Default::default()
            })
        );
        let mut plain = challenge("credentials");
        plain.secret_message.clear();
        assert_eq!(
            openvpn_answer(&plain, "alice", "", "ignored"),
            Ok(OpenVpnAnswer {
                username: "alice".into(),
                ..Default::default()
            })
        );
    }

    #[test]
    fn answers_reject_missing_input_and_unanswerable_kinds() {
        assert!(openvpn_answer(&challenge("credentials"), " ", "pw", "1").is_err());
        assert!(openvpn_answer(&challenge("credentials"), "alice", "pw", "").is_err());
        assert!(openvpn_answer(&challenge("secret"), "", "", "").is_err());
        assert!(openvpn_answer(&challenge("message"), "a", "b", "c").is_err());
        assert!(openvpn_answer(&challenge("open-url"), "a", "b", "c").is_err());
    }
}
