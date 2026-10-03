//! OpenConnect endpoints (`endpoints[]` of type `openconnect`):
//! `SubscribeOpenConnectStatus`, `SubmitOpenConnectAuthResponse`,
//! `CancelOpenConnectAuthChallenge`, plus the pure rules for answering an
//! authentication challenge. Also home of `VpnState`, which OpenVPN shares.
//!
//! How a challenge works upstream (sing-openconnect `auth_form.go`): while
//! the endpoint logs in, every form or browser step the server asks for that
//! the config (`username`, `password`, `token`, `form_entries`, …) can't
//! answer is published as the endpoint's one pending `auth_challenge`, and
//! the state reads `auth-pending`. The login waits — no timeout of its own —
//! until it is answered by id, cancelled (retried later, so it asks again),
//! or sing-box stops. Answering clears it at once; the next step, if any,
//! arrives as a new challenge with a new id.

use super::transport::{ApiError, IDLE_STREAM_READ_TIMEOUT};
use super::{pb, SingBoxApi};
use std::collections::BTreeMap;
use std::time::Duration;

/// Submitting or cancelling hands the answer to the waiting login (and a
/// cancel closes its continuation) — quick, but more than a plain read.
const AUTH_ACTION_TIMEOUT: Duration = Duration::from_secs(15);

impl SingBoxApi {
    /// Stream `SubscribeOpenConnectStatus`: on subscribe, one update with
    /// every OpenConnect endpoint (an empty list when the config has none),
    /// then a full update — all endpoints again — whenever any of them
    /// changes (state, challenge, transport). Idle otherwise: `TimedOut`
    /// after `IDLE_STREAM_READ_TIMEOUT`; re-subscribe.
    pub fn stream_openconnect_status(
        &self,
        mut on_update: impl FnMut(Vec<OpenConnectEndpointStatus>) -> bool,
    ) -> Result<(), ApiError> {
        self.stream(
            "SubscribeOpenConnectStatus",
            &(),
            IDLE_STREAM_READ_TIMEOUT,
            |update: pb::OpenConnectStatusUpdate| {
                on_update(
                    update
                        .endpoints
                        .into_iter()
                        .map(OpenConnectEndpointStatus::from_proto)
                        .collect(),
                )
            },
        )
    }

    /// Answer a form challenge. `values` must hold exactly the challenge's
    /// fields by submission key (see `openconnect_form_values`). Errors:
    /// `NOT_FOUND` (no such endpoint), `INVALID_ARGUMENT` (not an
    /// OpenConnect endpoint), `UNKNOWN` when the challenge is no longer
    /// pending or the values don't match the form.
    pub fn submit_openconnect_form(
        &self,
        endpoint_tag: &str,
        challenge_id: &str,
        values: BTreeMap<String, String>,
    ) -> Result<(), ApiError> {
        self.submit_openconnect(
            endpoint_tag,
            challenge_id,
            pb::open_connect_auth_response_submission::Response::Form(
                pb::OpenConnectAuthFormResponse { values },
            ),
        )
    }

    /// Answer a browser challenge with what the browser ended on (see
    /// `openconnect_callback_result`). Same errors as the form variant.
    pub fn submit_openconnect_browser(
        &self,
        endpoint_tag: &str,
        challenge_id: &str,
        result: &OpenConnectBrowserResult,
    ) -> Result<(), ApiError> {
        self.submit_openconnect(
            endpoint_tag,
            challenge_id,
            pb::open_connect_auth_response_submission::Response::Browser(result.to_proto()),
        )
    }

    fn submit_openconnect(
        &self,
        endpoint_tag: &str,
        challenge_id: &str,
        response: pb::open_connect_auth_response_submission::Response,
    ) -> Result<(), ApiError> {
        let request = pb::OpenConnectAuthResponseSubmission {
            endpoint_tag: endpoint_tag.to_string(),
            challenge_id: challenge_id.to_string(),
            response: Some(response),
        };
        self.unary_with_timeout(
            "SubmitOpenConnectAuthResponse",
            &request,
            AUTH_ACTION_TIMEOUT,
        )
    }

    /// `CancelOpenConnectAuthChallenge` — abandon this login attempt. Upstream
    /// treats that as a retryable failure: the endpoint stays `connecting`,
    /// backs off, logs in again and so asks again (a new challenge).
    pub fn cancel_openconnect_auth(
        &self,
        endpoint_tag: &str,
        challenge_id: &str,
    ) -> Result<(), ApiError> {
        let request = pb::OpenConnectAuthChallengeCancel {
            endpoint_tag: endpoint_tag.to_string(),
            challenge_id: challenge_id.to_string(),
        };
        self.unary_with_timeout(
            "CancelOpenConnectAuthChallenge",
            &request,
            AUTH_ACTION_TIMEOUT,
        )
    }
}

/// Lifecycle of an OpenConnect or OpenVPN client endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VpnState {
    Connecting,
    /// Waiting on the user: the status carries a challenge.
    AuthPending,
    Connected,
    /// Failed for good — the status's `error` says why. Only a sing-box
    /// restart tries again.
    Error,
    /// A value newer than this client.
    Other(String),
}

impl VpnState {
    pub(super) fn from_proto(value: &str) -> Self {
        match value {
            "connecting" => VpnState::Connecting,
            "auth-pending" => VpnState::AuthPending,
            "connected" => VpnState::Connected,
            "error" => VpnState::Error,
            other => VpnState::Other(other.to_string()),
        }
    }
}

/// One OpenConnect endpoint's status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenConnectEndpointStatus {
    pub endpoint_tag: String,
    pub state: VpnState,
    /// `state` as display text, in English.
    pub state_text: String,
    /// Why the endpoint gave up; empty unless `state` is `Error`.
    pub error: String,
    /// Set exactly while `state` is `AuthPending`.
    pub challenge: Option<OpenConnectChallenge>,
    /// Set exactly while `state` is `Connected`.
    pub tunnel: Option<OpenConnectTunnel>,
}

impl OpenConnectEndpointStatus {
    fn from_proto(status: pb::OpenConnectEndpointStatus) -> Self {
        Self {
            endpoint_tag: status.endpoint_tag,
            state: VpnState::from_proto(&status.state),
            state_text: status.state_text,
            error: status.error,
            challenge: status.auth_challenge.map(OpenConnectChallenge::from_proto),
            tunnel: status.tunnel_info.map(OpenConnectTunnel::from_proto),
        }
    }
}

/// The established tunnel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenConnectTunnel {
    pub server: String,
    /// `anyconnect`, `gp`, `fortinet`, `f5`, `pulse` or `nc`.
    pub flavor: String,
    /// The data channel in use (e.g. DTLS or TLS); may change while
    /// connected.
    pub transport: String,
    /// Assigned addresses as prefixes (`10.0.0.2/32`).
    pub ipv4: Vec<String>,
    pub ipv6: Vec<String>,
    pub dns: Vec<String>,
    pub mtu: u32,
    /// Unix seconds.
    pub connected_since: Option<i64>,
}

impl OpenConnectTunnel {
    fn from_proto(info: pb::OpenConnectTunnelInfo) -> Self {
        Self {
            server: info.server,
            flavor: info.flavor,
            transport: info.transport,
            ipv4: info.ipv4,
            ipv6: info.ipv6,
            dns: info.dns,
            mtu: info.mtu,
            connected_since: (info.connected_since > 0).then_some(info.connected_since),
        }
    }
}

/// A pending authentication step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenConnectChallenge {
    /// Answer or cancel by this id; a later step gets a new one.
    pub id: String,
    /// Server banner (login notice), possibly multi-line.
    pub banner: String,
    /// Server prompt for this step.
    pub message: String,
    /// Why the previous attempt failed (e.g. a wrong password); empty on the
    /// first try.
    pub error: String,
    pub prompt: OpenConnectPrompt,
}

impl OpenConnectChallenge {
    fn from_proto(challenge: pb::OpenConnectAuthChallenge) -> Self {
        use pb::open_connect_auth_challenge::Challenge;
        let prompt = match challenge.challenge {
            Some(Challenge::Form(form)) => OpenConnectPrompt::Form(
                form.fields
                    .into_iter()
                    .map(OpenConnectFormField::from_proto)
                    .collect(),
            ),
            Some(Challenge::Browser(browser)) => {
                OpenConnectPrompt::Browser(OpenConnectBrowserRequest::from_proto(browser))
            }
            None => OpenConnectPrompt::Unknown,
        };
        Self {
            id: challenge.id,
            banner: challenge.banner,
            message: challenge.message,
            error: challenge.error,
            prompt,
        }
    }
}

/// What the challenge asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenConnectPrompt {
    /// Fill in and submit these fields (`submit_openconnect_form`).
    Form(Vec<OpenConnectFormField>),
    /// Sign in through a web page (`submit_openconnect_browser`).
    Browser(OpenConnectBrowserRequest),
    /// Neither — a challenge kind newer than this client. It can still be
    /// cancelled.
    Unknown,
}

/// One visible form field. Hidden and token fields never reach the API:
/// sing-box fills those itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenConnectFormField {
    /// The key its answer is submitted under (unique within the form).
    pub submission_key: String,
    /// The server's field name (`username`, `password`, `group_list`, …).
    pub name: String,
    /// Server-provided label; may be empty.
    pub label: String,
    pub kind: OpenConnectFieldKind,
    /// Prefill: the server default, a configured or cached value.
    pub value: String,
    /// The choices of a `Select` field.
    pub options: Vec<OpenConnectChoice>,
}

impl OpenConnectFormField {
    fn from_proto(field: pb::OpenConnectAuthFormField) -> Self {
        Self {
            submission_key: field.submission_key,
            name: field.name,
            label: field.label,
            kind: OpenConnectFieldKind::from_proto(&field.kind),
            value: field.value,
            options: field
                .options
                .into_iter()
                .map(|choice| OpenConnectChoice {
                    value: choice.value,
                    label: choice.label,
                })
                .collect(),
        }
    }

    /// What to call the field in the UI: its label without the trailing
    /// colon servers like to add, else its name.
    pub fn display_label(&self) -> String {
        let label = self.label.trim().trim_end_matches(':').trim_end();
        if label.is_empty() {
            self.name.clone()
        } else {
            label.to_string()
        }
    }

    /// Index of the option to preselect for a `Select` field: the one
    /// matching the prefill, else the first. `None` when there are no
    /// options.
    pub fn initial_choice(&self) -> Option<usize> {
        if self.options.is_empty() {
            return None;
        }
        Some(
            self.options
                .iter()
                .position(|choice| choice.value == self.value)
                .unwrap_or(0),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenConnectFieldKind {
    Text,
    /// Mask the input.
    Password,
    /// Pick one of `options`; the answer is the option's `value`.
    Select,
    /// A kind newer than this client; treated as text.
    Other(String),
}

impl OpenConnectFieldKind {
    fn from_proto(kind: &str) -> Self {
        match kind {
            "text" => OpenConnectFieldKind::Text,
            "password" => OpenConnectFieldKind::Password,
            "select" => OpenConnectFieldKind::Select,
            other => OpenConnectFieldKind::Other(other.to_string()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenConnectChoice {
    /// Submitted when chosen.
    pub value: String,
    /// Shown; may be empty (then show `value`).
    pub label: String,
}

impl OpenConnectChoice {
    pub fn display_label(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.value
        } else {
            &self.label
        }
    }
}

/// A web sign-in (SSO / SAML). Exactly one completion mode is set — see
/// `mode()`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenConnectBrowserRequest {
    /// Where the sign-in starts.
    pub url: String,
    /// Cookie mode: the page whose cookies finish the sign-in.
    pub final_url: String,
    /// Cookie mode: the cookies to capture there.
    pub cookie_names: Vec<String>,
    /// Cookie mode: a cookie that, set at any point, ends the sign-in early
    /// (an error report).
    pub early_cookie_names: Vec<String>,
    /// Header mode: response headers to capture.
    pub header_names: Vec<String>,
    /// Callback mode: the sign-in ends when the browser is sent to a URL
    /// starting with one of these.
    pub callback_url_prefixes: Vec<String>,
}

impl OpenConnectBrowserRequest {
    fn from_proto(request: pb::OpenConnectBrowserRequest) -> Self {
        // `cache_id` only keys an embedded browser's cookie jar; unused here.
        Self {
            url: request.url,
            final_url: request.final_url,
            cookie_names: request.cookie_names,
            early_cookie_names: request.early_cookie_names,
            header_names: request.header_names,
            callback_url_prefixes: request.callback_url_prefixes,
        }
    }

    /// How the sign-in completes, by upstream `validateBrowserRequest`'s
    /// rules (callback, then cookie, then header fields decide).
    pub fn mode(&self) -> OpenConnectBrowserMode {
        if !self.callback_url_prefixes.is_empty() {
            OpenConnectBrowserMode::Callback
        } else if !self.final_url.is_empty()
            || !self.cookie_names.is_empty()
            || !self.early_cookie_names.is_empty()
        {
            OpenConnectBrowserMode::Cookies
        } else {
            OpenConnectBrowserMode::Headers
        }
    }
}

/// How a browser sign-in hands its result back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenConnectBrowserMode {
    /// The page redirects to a callback URL (Fortinet SAML:
    /// `http://127.0.0.1:<port>/?id=…`). The answer is that URL — something
    /// a user can copy out of any browser's address bar.
    Callback,
    /// The answer is cookies set on `final_url` (AnyConnect SSO). They are
    /// typically HttpOnly, so only an embedded browser can capture them.
    Cookies,
    /// The answer is response headers (GlobalProtect SAML). Only an
    /// embedded browser can capture them.
    Headers,
}

/// What a browser sign-in ended on. Exactly the parts its mode asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenConnectBrowserResult {
    pub final_url: String,
    /// `(name, value)`.
    pub cookies: Vec<(String, String)>,
    /// `(name, values)`.
    pub headers: Vec<(String, Vec<String>)>,
}

impl OpenConnectBrowserResult {
    fn to_proto(&self) -> pb::OpenConnectBrowserResult {
        pb::OpenConnectBrowserResult {
            final_url: self.final_url.clone(),
            cookies: self
                .cookies
                .iter()
                .map(|(name, value)| pb::OpenConnectBrowserCookie {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
            headers: self
                .headers
                .iter()
                .map(|(name, values)| pb::OpenConnectBrowserHeader {
                    name: name.clone(),
                    values: values.clone(),
                })
                .collect(),
        }
    }
}

/// Build a form answer: one value per field, by submission key — exactly
/// the set upstream `validateAuthFormValues` accepts (no key missing, none
/// extra, a select value among its options). `answers[i]` answers
/// `fields[i]`; for a select it is the chosen option's `value`. Text is
/// passed through untrimmed (passwords may have edge spaces).
pub fn openconnect_form_values(
    fields: &[OpenConnectFormField],
    answers: &[String],
) -> Result<BTreeMap<String, String>, String> {
    if fields.len() != answers.len() {
        return Err(crate::i18n::s().vpn.form_changed.to_string());
    }
    let mut values = BTreeMap::new();
    for (field, answer) in fields.iter().zip(answers) {
        if field.kind == OpenConnectFieldKind::Select
            && !field.options.iter().any(|choice| &choice.value == answer)
        {
            return Err((crate::i18n::s().vpn.choose_value)(&field.display_label()));
        }
        values.insert(field.submission_key.clone(), answer.clone());
    }
    Ok(values)
}

/// Build a callback-mode browser answer from the address the user copied
/// out of the browser. Upstream accepts only a URL starting with one of the
/// request's callback prefixes, with no cookies or headers.
pub fn openconnect_callback_result(
    request: &OpenConnectBrowserRequest,
    pasted: &str,
) -> Result<OpenConnectBrowserResult, String> {
    let url = pasted.trim();
    if url.is_empty() {
        return Err(crate::i18n::s().vpn.paste_address.to_string());
    }
    if !request
        .callback_url_prefixes
        .iter()
        .any(|prefix| url.starts_with(prefix.as_str()))
    {
        let t = &crate::i18n::s().vpn;
        return Err((t.address_mismatch)(
            &request.callback_url_prefixes.join(t.or),
        ));
    }
    Ok(OpenConnectBrowserResult {
        final_url: url.to_string(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(key: &str, kind: OpenConnectFieldKind) -> OpenConnectFormField {
        OpenConnectFormField {
            submission_key: key.into(),
            name: key.into(),
            label: String::new(),
            kind,
            value: String::new(),
            options: Vec::new(),
        }
    }

    fn select(key: &str, values: &[&str], prefill: &str) -> OpenConnectFormField {
        OpenConnectFormField {
            options: values
                .iter()
                .map(|value| OpenConnectChoice {
                    value: value.to_string(),
                    label: String::new(),
                })
                .collect(),
            value: prefill.into(),
            ..field(key, OpenConnectFieldKind::Select)
        }
    }

    #[test]
    fn status_maps_every_field() {
        let status = OpenConnectEndpointStatus::from_proto(pb::OpenConnectEndpointStatus {
            endpoint_tag: "corp".into(),
            state: "connected".into(),
            state_text: "Connected".into(),
            auth_challenge: None,
            error: String::new(),
            tunnel_info: Some(pb::OpenConnectTunnelInfo {
                server: "vpn.example.com".into(),
                flavor: "anyconnect".into(),
                transport: "DTLS".into(),
                ipv4: vec!["10.0.0.2/32".into()],
                ipv6: vec!["fd00::2/128".into()],
                dns: vec!["10.0.0.1".into()],
                mtu: 1400,
                connected_since: 1_800_000_000,
            }),
        });
        assert_eq!(
            status,
            OpenConnectEndpointStatus {
                endpoint_tag: "corp".into(),
                state: VpnState::Connected,
                state_text: "Connected".into(),
                error: String::new(),
                challenge: None,
                tunnel: Some(OpenConnectTunnel {
                    server: "vpn.example.com".into(),
                    flavor: "anyconnect".into(),
                    transport: "DTLS".into(),
                    ipv4: vec!["10.0.0.2/32".into()],
                    ipv6: vec!["fd00::2/128".into()],
                    dns: vec!["10.0.0.1".into()],
                    mtu: 1400,
                    connected_since: Some(1_800_000_000),
                }),
            }
        );
        let unknown_since = OpenConnectTunnel::from_proto(pb::OpenConnectTunnelInfo::default());
        assert_eq!(unknown_since.connected_since, None);
    }

    #[test]
    fn states_map_upstream_strings() {
        let states: Vec<VpnState> = ["connecting", "auth-pending", "connected", "error", "x"]
            .into_iter()
            .map(VpnState::from_proto)
            .collect();
        assert_eq!(
            states,
            vec![
                VpnState::Connecting,
                VpnState::AuthPending,
                VpnState::Connected,
                VpnState::Error,
                VpnState::Other("x".into()),
            ]
        );
    }

    #[test]
    fn form_challenge_maps_fields_and_choices() {
        let challenge = OpenConnectChallenge::from_proto(pb::OpenConnectAuthChallenge {
            id: "3".into(),
            banner: "Authorized use only".into(),
            message: "Please enter your credentials".into(),
            error: "Login failed".into(),
            challenge: Some(pb::open_connect_auth_challenge::Challenge::Form(
                pb::OpenConnectAuthForm {
                    fields: vec![
                        pb::OpenConnectAuthFormField {
                            submission_key: "main:group_list:0".into(),
                            name: "group_list".into(),
                            label: "GROUP:".into(),
                            kind: "select".into(),
                            value: "b".into(),
                            options: vec![
                                pb::OpenConnectAuthFormChoice {
                                    value: "a".into(),
                                    label: "Staff".into(),
                                },
                                pb::OpenConnectAuthFormChoice {
                                    value: "b".into(),
                                    label: String::new(),
                                },
                            ],
                        },
                        pb::OpenConnectAuthFormField {
                            submission_key: "main:password:1".into(),
                            name: "password".into(),
                            label: String::new(),
                            kind: "password".into(),
                            value: String::new(),
                            options: Vec::new(),
                        },
                    ],
                },
            )),
        });
        assert_eq!(challenge.id, "3");
        assert_eq!(challenge.banner, "Authorized use only");
        assert_eq!(challenge.message, "Please enter your credentials");
        assert_eq!(challenge.error, "Login failed");
        let OpenConnectPrompt::Form(fields) = challenge.prompt else {
            panic!("expected a form");
        };
        assert_eq!(fields[0].kind, OpenConnectFieldKind::Select);
        assert_eq!(fields[0].display_label(), "GROUP");
        assert_eq!(fields[0].initial_choice(), Some(1));
        assert_eq!(fields[0].options[0].display_label(), "Staff");
        assert_eq!(fields[0].options[1].display_label(), "b");
        assert_eq!(fields[1].kind, OpenConnectFieldKind::Password);
        assert_eq!(fields[1].display_label(), "password", "falls back to name");
        assert_eq!(fields[1].initial_choice(), None);
    }

    #[test]
    fn browser_challenge_maps_and_classifies_modes() {
        let challenge = OpenConnectChallenge::from_proto(pb::OpenConnectAuthChallenge {
            id: "4".into(),
            challenge: Some(pb::open_connect_auth_challenge::Challenge::Browser(
                pb::OpenConnectBrowserRequest {
                    url: "https://vpn.example.com/remote/saml/start?redirect=1".into(),
                    callback_url_prefixes: vec!["http://127.0.0.1:".into()],
                    cache_id: "c".into(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        });
        let OpenConnectPrompt::Browser(request) = challenge.prompt else {
            panic!("expected a browser request");
        };
        assert_eq!(request.mode(), OpenConnectBrowserMode::Callback);
        assert_eq!(request.callback_url_prefixes, vec!["http://127.0.0.1:"]);

        let cookies = OpenConnectBrowserRequest {
            url: "u".into(),
            final_url: "https://vpn.example.com/+CSCOE+/saml_ac_login.html".into(),
            cookie_names: vec!["acSamlv2Token".into()],
            early_cookie_names: vec!["acSamlv2Error".into()],
            ..Default::default()
        };
        assert_eq!(cookies.mode(), OpenConnectBrowserMode::Cookies);
        let headers = OpenConnectBrowserRequest {
            url: "u".into(),
            header_names: vec!["prelogin-cookie".into()],
            ..Default::default()
        };
        assert_eq!(headers.mode(), OpenConnectBrowserMode::Headers);

        let none = OpenConnectChallenge::from_proto(pb::OpenConnectAuthChallenge::default());
        assert_eq!(none.prompt, OpenConnectPrompt::Unknown);
    }

    #[test]
    fn form_values_cover_every_field_by_submission_key() {
        let fields = vec![
            field("u", OpenConnectFieldKind::Text),
            field("p", OpenConnectFieldKind::Password),
            select("g", &["a", "b"], ""),
        ];
        let values = openconnect_form_values(
            &fields,
            &[" alice".to_string(), "pw ".to_string(), "b".to_string()],
        )
        .unwrap();
        assert_eq!(
            values.into_iter().collect::<Vec<_>>(),
            vec![
                ("g".to_string(), "b".to_string()),
                ("p".to_string(), "pw ".to_string()),
                ("u".to_string(), " alice".to_string()),
            ]
        );
        // Empty text is a valid answer (upstream only checks presence).
        assert!(openconnect_form_values(&fields[..1], &[String::new()]).is_ok());
        assert!(openconnect_form_values(&[], &[]).unwrap().is_empty());
    }

    #[test]
    fn form_values_reject_bad_selects_and_mismatched_answers() {
        let fields = vec![select("g", &["a"], "a")];
        assert!(openconnect_form_values(&fields, &["z".to_string()]).is_err());
        assert!(openconnect_form_values(&fields, &[]).is_err());
    }

    #[test]
    fn callback_result_requires_an_accepted_prefix() {
        let request = OpenConnectBrowserRequest {
            url: "https://vpn.example.com/remote/saml/start".into(),
            callback_url_prefixes: vec!["http://127.0.0.1:".into()],
            ..Default::default()
        };
        assert_eq!(
            openconnect_callback_result(&request, "  http://127.0.0.1:8020/?id=abc \n"),
            Ok(OpenConnectBrowserResult {
                final_url: "http://127.0.0.1:8020/?id=abc".into(),
                cookies: Vec::new(),
                headers: Vec::new(),
            })
        );
        assert!(openconnect_callback_result(&request, "").is_err());
        assert!(openconnect_callback_result(&request, "https://evil.example/?id=1").is_err());
    }

    #[test]
    fn browser_result_encodes_cookies_and_headers() {
        let result = OpenConnectBrowserResult {
            final_url: "u".into(),
            cookies: vec![("n".into(), "v".into())],
            headers: vec![("h".into(), vec!["x".into(), "y".into()])],
        }
        .to_proto();
        assert_eq!(result.final_url, "u");
        assert_eq!(result.cookies[0].name, "n");
        assert_eq!(result.cookies[0].value, "v");
        assert_eq!(result.headers[0].name, "h");
        assert_eq!(result.headers[0].values, vec!["x", "y"]);
    }
}
