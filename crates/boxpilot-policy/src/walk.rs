//! The walk over a profile's config: what each key is, judged by where it
//! stands. Every rule names keys exactly, in lower case; sing-box 1.14.2's
//! `option/` tree is the reference for each table entry.

use crate::{
    is_attachment_id, AttachmentRef, DropReason, Dropped, Expected, LocalFileField, Refusal,
    RefusalKind, TailscaleEndpoint, ATTACHMENT_PREFIX,
};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

/// Deeper than this the walk stops and refuses: serde_json's own recursion
/// limit, which no value parsed from text exceeds. It guards `local_file_fields`
/// and `attach` on a value built in code; `check` has already held the text
/// to the tighter `Limits::max_depth`.
const WALK_DEPTH_LIMIT: usize = 128;

/// Whether `text` nests objects and arrays deeper than `limit`, judged from
/// the raw text so the check costs no allocation. Brackets inside strings
/// don't count; a multi-byte UTF-8 sequence never contains `"`, `\` or a
/// bracket byte.
pub(crate) fn nesting_exceeds(text: &str, limit: usize) -> bool {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in text.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > limit {
                    return true;
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

/// Whether sing-box can only read `key` as written. Its JSON decoder (a fork
/// of Go's `encoding/json`, `sing/common/json/internal/contextjson`) falls
/// back to a case-insensitive match under Unicode simple folding, where `ſ`
/// (U+017F) is `s` and the Kelvin sign (U+212A) is `k`: to sing-box,
/// `Executable_Path` and `ſtate_directory` are tor's `executable_path` and
/// Tailscale's `state_directory`. sing-box's own field names are all
/// lower-case ASCII, so a key with an upper-case ASCII letter or any
/// non-ASCII character is refused instead of guessed at. Today only those
/// two characters fold to ASCII; refusing all of non-ASCII keeps that true
/// whatever a future Go version folds, and costs nothing, since no field
/// sing-box knows is spelled that way.
fn is_canonical(key: &str) -> bool {
    key.bytes().all(|b| b.is_ascii() && !b.is_ascii_uppercase())
}

/// Whether a key looks like a filesystem location (ADR 0006: deny by shape).
/// Wider than today's schema needs: plurals and `dir` cost nothing now, and
/// catch the field upstream adds next.
fn is_path_shaped(key: &str) -> bool {
    const NAMES: [&str; 10] = [
        "path",
        "paths",
        "dir",
        "dirs",
        "directory",
        "directories",
        "file",
        "files",
        "output",
        "pid_file",
    ];
    const SUFFIXES: [&str; 9] = [
        "_path",
        "_paths",
        "_dir",
        "_dirs",
        "_directory",
        "_directories",
        "_file",
        "_files",
        "_output",
    ];
    NAMES.contains(&key) || SUFFIXES.iter().any(|suffix| key.ends_with(suffix))
}

/// The keys of shadowsocks `plugin_opts`, unescaped: a port of sing-box's
/// `ParsePluginOptions` (`transport/sip003/args.go`), so both read the same
/// keys. `None` where that parse fails too.
fn plugin_option_keys(opts: &str) -> Option<Vec<Vec<u8>>> {
    /// Up to the first unescaped `term` byte: its index, and the text before
    /// it with escapes removed. `None` for a trailing lone `\`.
    fn until(s: &[u8], term: &[u8]) -> Option<(usize, Vec<u8>)> {
        let mut unescaped = Vec::new();
        let mut i = 0;
        while i < s.len() {
            let mut byte = s[i];
            if term.contains(&byte) {
                break;
            }
            if byte == b'\\' {
                i += 1;
                byte = *s.get(i)?;
            }
            unescaped.push(byte);
            i += 1;
        }
        Some((i, unescaped))
    }
    let s = opts.as_bytes();
    let mut keys = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let (offset, key) = until(&s[i..], b"=;")?;
        if key.is_empty() {
            return None;
        }
        i += offset;
        if i < s.len() && s[i] == b'=' {
            let (offset, _value) = until(&s[i + 1..], b";")?;
            i += 1 + offset;
        }
        keys.push(key);
        // Past the `;`.
        i += 1;
    }
    Some(keys)
}

/// JSON Pointer (RFC 6901) to `parent`'s child `key`.
fn child(parent: &str, key: &str) -> String {
    let mut pointer = String::with_capacity(parent.len() + key.len() + 1);
    pointer.push_str(parent);
    pointer.push('/');
    for c in key.chars() {
        match c {
            '~' => pointer.push_str("~0"),
            '/' => pointer.push_str("~1"),
            c => pointer.push(c),
        }
    }
    pointer
}

fn type_of(object: &Map<String, Value>) -> Option<&str> {
    object.get("type").and_then(Value::as_str)
}

/// What an object is, by its place and `type`. A type the policy has no
/// rules for gets the shape rule alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// Anything without rules of its own.
    Generic,
    /// `log`.
    Log,
    /// `dns`.
    Dns,
    /// `dns.servers[]`.
    DnsServer(DnsServerKind),
    /// A route or DNS rule, a logical rule's `rules[]`, an inline rule set's
    /// headless rules.
    Rule,
    /// `ntp`.
    Ntp,
    /// The top-level `certificate`.
    Certificate,
    /// `outbounds[]`.
    Outbound(OutboundKind),
    /// `endpoints[]`.
    Endpoint(EndpointKind),
    /// A V2Ray `transport` (vmess, vless, trojan).
    Transport(TransportKind),
    /// A `tls` object.
    Tls(TlsKind),
    /// An outbound TLS `ech`.
    Ech,
    /// OpenVPN `tls.control_wrap`.
    ControlWrap,
    /// OpenConnect `token`.
    OpenConnectToken,
    /// OpenConnect `tncc`.
    Tncc,
    /// OpenConnect `tncc.certificates[]`.
    TnccCertificate,
    /// `route`.
    Route,
    /// `route.rule_set[]`.
    RuleSet(RuleSetKind),
    /// A remote rule set's inline `http_client`.
    HttpClient,
    /// `experimental.cache_file`.
    CacheFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DnsServerKind {
    /// `https`, `h3`: DNS over HTTP, whose `path` is the request's URL path.
    Http,
    /// `hosts`: its `path` lists hosts files to read.
    Hosts,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutboundKind {
    /// Starts the tor executable.
    Tor,
    /// `http`: its `path` is the CONNECT request's URL path.
    Http,
    /// `ssh`: may read a private key file.
    Ssh,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointKind {
    /// No tor endpoint exists; refused like the outbound should it appear.
    Tor,
    Tailscale,
    OpenConnect,
    OpenVpnClient,
    OpenVpnServer,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransportKind {
    /// `http`, `ws`, `httpupgrade`: their `path` is the request's URL path.
    Http,
    /// `grpc`, `quic`: no path at all.
    Other,
}

/// Which `tls` struct: each takes its own file fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TlsKind {
    /// `OutboundTLSOptions`: outbounds, DNS servers, HTTP clients.
    Outbound,
    /// `OpenConnectTLSOptions`.
    OpenConnect,
    /// `OpenVPNOutboundTLSOptions`.
    OpenVpnClient,
    /// `OpenVPNInboundTLSOptions`.
    OpenVpnServer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleSetKind {
    /// `inline`, or no `type`: headless rules.
    Inline,
    /// `local`: a rule-set file to read.
    Local,
    /// `remote`: downloaded into the cache file; may start from a local file.
    Remote,
    Other,
}

impl OutboundKind {
    fn of(object: &Map<String, Value>) -> Self {
        match type_of(object) {
            Some("tor") => Self::Tor,
            Some("http") => Self::Http,
            Some("ssh") => Self::Ssh,
            _ => Self::Other,
        }
    }
}

impl EndpointKind {
    fn of(object: &Map<String, Value>) -> Self {
        match type_of(object) {
            Some("tor") => Self::Tor,
            Some("tailscale") => Self::Tailscale,
            Some("openconnect") => Self::OpenConnect,
            Some("openvpn-client") => Self::OpenVpnClient,
            Some("openvpn-server") => Self::OpenVpnServer,
            _ => Self::Other,
        }
    }
}

impl DnsServerKind {
    fn of(object: &Map<String, Value>) -> Self {
        match type_of(object) {
            Some("https" | "h3") => Self::Http,
            Some("hosts") => Self::Hosts,
            _ => Self::Other,
        }
    }
}

impl TransportKind {
    fn of(object: &Map<String, Value>) -> Self {
        match type_of(object) {
            Some("http" | "ws" | "httpupgrade") => Self::Http,
            _ => Self::Other,
        }
    }
}

impl RuleSetKind {
    fn of(object: &Map<String, Value>) -> Self {
        match object.get("type") {
            None => Self::Inline,
            Some(Value::String(kind)) => match kind.as_str() {
                "" | "inline" => Self::Inline,
                "local" => Self::Local,
                "remote" => Self::Remote,
                _ => Self::Other,
            },
            Some(_) => Self::Other,
        }
    }
}

/// How to walk a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Node {
    /// Objects get the shape rule alone; arrays are walked element by
    /// element; scalars are fine.
    Plain,
    /// As `Plain`, but an object gets this scope: a remote rule set's
    /// `http_client` is a tag or an object.
    PlainOr(Scope),
    /// An object with these rules; anything else is malformed.
    Object(Scope),
    /// A V2Ray transport object, whose rules depend on its `type`.
    Transport,
    /// A map whose keys are data (header names, domains, interface names),
    /// not fields: no case or shape rule. sing-box's maps hold strings or
    /// lists, so an object or nested list in one is refused, not walked.
    Map,
    /// An array of objects of this kind.
    ArrayOf(Element),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Element {
    Outbound,
    Endpoint,
    DnsServer,
    Rule,
    RuleSet,
    TnccCertificate,
}

/// Whether a file-read field takes one path or, as sing-box's `Listable`, a
/// path or a list of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arity {
    One,
    Many,
}

/// What to do with one key.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// Remove it: nothing runs it on the privileged path, or the helper
    /// fills it.
    Drop(DropReason),
    Refuse(RefusalKind),
    /// A file sing-box reads: it must hold attachment references.
    Read(Arity),
    /// Walk into the value.
    Visit(Node),
}

/// A key that matches the shape rule but names no file.
const NOT_A_FILE: Action = Action::Visit(Node::Plain);

/// The rule for `key` in an object of `scope`, the key already known to be
/// canonical. Features that reach beyond networking come first, wherever
/// they stand; then the context-scoped table; then the shape rule.
fn action(scope: Scope, key: &str, value: &Value) -> Action {
    // The tor outbound's `executable_path` ("The path to the Tor
    // executable"), and anything like it that turns up elsewhere.
    if key == "executable_path" {
        return Action::Refuse(RefusalKind::RunsProgram);
    }
    // OpenConnect `csd` / `hip` / `tncc` `wrapper_path`: "Path to an
    // external … wrapper executable".
    if key.ends_with("wrapper_path") {
        return Action::Refuse(RefusalKind::RunsProgram);
    }
    // The Tailscale endpoint's `ssh_server` (since 1.14.0): `true`, or
    // `{"enabled": true, …}`. Run as root, it maps tailnet users to any
    // local account.
    if key == "ssh_server" && !ssh_server_off(value) {
        return Action::Refuse(RefusalKind::SystemChange);
    }
    // Dial field `netns`: a network namespace to dial from, opened as root:
    // by path when it starts with `/` (`common/listener/listener.go`),
    // otherwise a `network_namespaces` tag (refused with its section) or a
    // name under `/run/netns`. Linux only, but the policy doesn't depend on
    // the helper's OS. Empty means none.
    if key == "netns" && !matches!(value, Value::Null) && value.as_str() != Some("") {
        return Action::Refuse(RefusalKind::FilesystemPath);
    }
    // Shadowsocks `plugin_opts`: v2ray-plugin reads its `cert=` option as a
    // certificate file path (`transport/sip003/v2ray.go`), a path inside a
    // string where no key shows it.
    if key == "plugin_opts" {
        return match value {
            Value::Null => Action::Visit(Node::Plain),
            Value::String(opts) => match plugin_option_keys(opts) {
                None => Action::Refuse(RefusalKind::Malformed {
                    expected: Expected::PluginOptions,
                }),
                Some(keys) if keys.iter().any(|key| key == b"cert") => {
                    Action::Refuse(RefusalKind::FilesystemPath)
                }
                Some(_) => Action::Visit(Node::Plain),
            },
            _ => Action::Refuse(RefusalKind::Malformed {
                expected: Expected::String,
            }),
        };
    }
    if let Some(rule) = scoped(scope, key, value) {
        return rule;
    }
    if is_path_shaped(key) {
        return Action::Refuse(RefusalKind::FilesystemPath);
    }
    Action::Visit(Node::Plain)
}

/// Whether a Tailscale `ssh_server` value leaves the server off: `false`,
/// or an object whose `enabled` isn't `true`. Its other keys are walked as
/// usual, so a differently-cased `Enabled` is refused there.
fn ssh_server_off(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => true,
        Value::Object(object) => matches!(
            object.get("enabled"),
            None | Some(Value::Null) | Some(Value::Bool(false))
        ),
        _ => false,
    }
}

/// The context-scoped rules: what a key is where it stands. `None` leaves
/// the key to the shape rule.
fn scoped(scope: Scope, key: &str, value: &Value) -> Option<Action> {
    use Scope::*;
    let rule = match (scope, key) {
        // `log.output`: a log file. Logs go to stdout, which the helper
        // keeps for `Logs`.
        (Log, "output") => Action::Drop(DropReason::HelperOwned),

        (Dns, "servers") => Action::Visit(Node::ArrayOf(Element::DnsServer)),
        (Dns, "rules") => Action::Visit(Node::ArrayOf(Element::Rule)),
        // https / h3 DNS server `path`: the DoH request's URL path
        // (`RemoteHTTPSDNSServerOptions`).
        (DnsServer(DnsServerKind::Http), "path") => NOT_A_FILE,
        // hosts DNS server `path`: hosts files to read, a string or a list
        // (`HostsDNSServerOptions`).
        (DnsServer(DnsServerKind::Hosts), "path") => Action::Read(Arity::Many),
        // hosts DNS server `predefined`: domain → addresses.
        (DnsServer(DnsServerKind::Hosts), "predefined") => Action::Visit(Node::Map),
        // tls / https / quic / h3 DNS servers: outbound TLS.
        (DnsServer(_), "tls") => Action::Visit(Node::Object(Tls(TlsKind::Outbound))),
        // https / h3 DNS server request headers.
        (DnsServer(_), "headers") => Action::Visit(Node::Map),

        // Rule matcher `process_path`: compared with the path of the process
        // that owns a connection; never opened. (`process_path_regex` doesn't
        // match the shape rule.)
        (Rule, "process_path") => NOT_A_FILE,
        // A logical rule's sub-rules.
        (Rule, "rules") => Action::Visit(Node::ArrayOf(Element::Rule)),
        // Rule matchers keyed by interface name / interface type.
        (Rule, "interface_address" | "network_interface_address") => Action::Visit(Node::Map),

        // NTP `write_to_system`: sets the system clock.
        (Ntp, "write_to_system") => {
            if *value == Value::Bool(false) {
                Action::Visit(Node::Plain)
            } else {
                Action::Refuse(RefusalKind::SystemChange)
            }
        }

        // Top-level `certificate.certificate_path`: CA files to trust, a
        // string or a list.
        (Certificate, "certificate_path") => Action::Read(Arity::Many),
        // `certificate.certificate_directory_path`: directories of CA files.
        (Certificate, "certificate_directory_path") => Action::Refuse(RefusalKind::Directory),

        // The tor outbound starts the tor executable, whatever its fields.
        (Outbound(OutboundKind::Tor) | Endpoint(EndpointKind::Tor), "type") => {
            Action::Refuse(RefusalKind::RunsProgram)
        }
        // tor `torrc`: options for tor, by tor's own (mixed-case) names.
        (Outbound(OutboundKind::Tor), "torrc") => Action::Visit(Node::Map),
        // `http` outbound `path`: the request's URL path
        // (`HTTPOutboundOptions`).
        (Outbound(OutboundKind::Http), "path") => NOT_A_FILE,
        // `ssh` outbound `private_key_path`: a private key file.
        (Outbound(OutboundKind::Ssh), "private_key_path") => Action::Read(Arity::One),
        (Outbound(_), "tls") => Action::Visit(Node::Object(Tls(TlsKind::Outbound))),
        (Outbound(_), "transport") => Action::Visit(Node::Transport),
        // HTTP outbound `headers`, naive `extra_headers`.
        (Outbound(_), "headers" | "extra_headers") => Action::Visit(Node::Map),

        // Tailscale `state_directory` and `taildrop_directory`: the helper
        // keeps both in the owner's state directory (`materialize`).
        (Endpoint(EndpointKind::Tailscale), "state_directory" | "taildrop_directory") => {
            Action::Drop(DropReason::HelperOwned)
        }
        (Endpoint(EndpointKind::OpenConnect), "tls") => {
            Action::Visit(Node::Object(Tls(TlsKind::OpenConnect)))
        }
        (Endpoint(EndpointKind::OpenConnect), "token") => {
            Action::Visit(Node::Object(OpenConnectToken))
        }
        (Endpoint(EndpointKind::OpenConnect), "tncc") => Action::Visit(Node::Object(Tncc)),
        (Endpoint(EndpointKind::OpenVpnClient), "tls") => {
            Action::Visit(Node::Object(Tls(TlsKind::OpenVpnClient)))
        }
        (Endpoint(EndpointKind::OpenVpnServer), "tls") => {
            Action::Visit(Node::Object(Tls(TlsKind::OpenVpnServer)))
        }
        // OpenVPN `static_key_path`: a static key file.
        (
            Endpoint(EndpointKind::OpenVpnClient | EndpointKind::OpenVpnServer),
            "static_key_path",
        ) => Action::Read(Arity::One),

        // V2Ray http / ws / httpupgrade transport `path`: the request's URL
        // path (`V2RayHTTPOptions`, `V2RayWebsocketOptions`,
        // `V2RayHTTPUpgradeOptions`).
        (Transport(TransportKind::Http), "path") => NOT_A_FILE,
        (Transport(_), "headers") => Action::Visit(Node::Map),

        // `OutboundTLSOptions`: the CA certificate, client certificate and
        // client key files, all single paths.
        (
            Tls(TlsKind::Outbound),
            "certificate_path" | "client_certificate_path" | "client_key_path",
        ) => Action::Read(Arity::One),
        (Tls(TlsKind::Outbound), "ech") => Action::Visit(Node::Object(Ech)),
        // `OpenConnectTLSOptions`: CA, client certificate and key, and the
        // machine (MCA) certificate and key files.
        (
            Tls(TlsKind::OpenConnect),
            "certificate_authority_path"
            | "client_certificate_path"
            | "client_key_path"
            | "mca_certificate_path"
            | "mca_key_path",
        ) => Action::Read(Arity::One),
        // `OpenVPNOutboundTLSOptions`: CA, client certificate and key, CRL.
        (
            Tls(TlsKind::OpenVpnClient),
            "certificate_path" | "client_certificate_path" | "client_key_path" | "crl_path",
        ) => Action::Read(Arity::One),
        // `OpenVPNInboundTLSOptions`: server certificate and key, client CA,
        // CRL.
        (
            Tls(TlsKind::OpenVpnServer),
            "certificate_path" | "key_path" | "client_certificate_path" | "crl_path",
        ) => Action::Read(Arity::One),
        (Tls(TlsKind::OpenVpnClient | TlsKind::OpenVpnServer), "control_wrap") => {
            Action::Visit(Node::Object(ControlWrap))
        }
        // Outbound ECH `config_path`: an ECH config file.
        (Ech, "config_path") => Action::Read(Arity::One),
        // OpenVPN `control_wrap.key_path`: a tls-auth / tls-crypt key file.
        (ControlWrap, "key_path") => Action::Read(Arity::One),
        // OpenConnect `token.secret_path`: a token secret or OIDC token file.
        (OpenConnectToken, "secret_path") => Action::Read(Arity::One),
        (Tncc, "certificates") => Action::Visit(Node::ArrayOf(Element::TnccCertificate)),
        // OpenConnect `tncc.certificates[].certificate_path`: a certificate
        // file.
        (TnccCertificate, "certificate_path") => Action::Read(Arity::One),

        (Route, "rules") => Action::Visit(Node::ArrayOf(Element::Rule)),
        (Route, "rule_set") => Action::Visit(Node::ArrayOf(Element::RuleSet)),
        // Local rule set `path`: the rule-set file.
        (RuleSet(RuleSetKind::Local), "path") => Action::Read(Arity::One),
        // Remote rule set `initial_path` (since 1.14.0): read once at start
        // when the cache file has no copy yet.
        (RuleSet(RuleSetKind::Remote), "initial_path") => Action::Read(Arity::One),
        (RuleSet(RuleSetKind::Remote), "http_client") => Action::Visit(Node::PlainOr(HttpClient)),
        // An inline rule set's headless rules.
        (RuleSet(RuleSetKind::Inline), "rules") => Action::Visit(Node::ArrayOf(Element::Rule)),
        (HttpClient, "tls") => Action::Visit(Node::Object(Tls(TlsKind::Outbound))),
        (HttpClient, "headers") => Action::Visit(Node::Map),

        // `experimental.cache_file.path`: the helper sets its own.
        (CacheFile, "path") => Action::Drop(DropReason::HelperOwned),

        // Dial field `tcp_multi_path`: a boolean, MPTCP on or off. Dial
        // fields sit in outbounds, endpoints, DNS servers, `ntp`, the
        // route `direct` action and HTTP clients.
        (Outbound(_) | Endpoint(_) | DnsServer(_) | Ntp | Rule | HttpClient, "tcp_multi_path") => {
            NOT_A_FILE
        }

        _ => return None,
    };
    Some(rule)
}

/// One walk over a config: refusals, drops, and the attachment references,
/// local files and Tailscale endpoints it found. It only reads the config;
/// `check` applies `removals` afterwards, so the GUI's `local_file_fields`
/// walks the caller's value as it is.
pub(crate) struct Walk<'a> {
    attachments: &'a BTreeSet<String>,
    pub refusals: Vec<Refusal>,
    pub dropped: Vec<Dropped>,
    /// Each dropped key, as (pointer to its object, key).
    pub removals: Vec<(String, String)>,
    pub local_files: Vec<LocalFileField>,
    pub refs: Vec<AttachmentRef>,
    pub tailscale: Vec<TailscaleEndpoint>,
}

impl<'a> Walk<'a> {
    pub fn new(attachments: &'a BTreeSet<String>) -> Self {
        Self {
            attachments,
            refusals: Vec::new(),
            dropped: Vec::new(),
            removals: Vec::new(),
            local_files: Vec::new(),
            refs: Vec::new(),
            tailscale: Vec::new(),
        }
    }

    fn refuse(&mut self, pointer: String, kind: RefusalKind) {
        self.refusals.push(Refusal { pointer, kind });
    }

    /// Note `key` of the object at `parent` as dropped, to be removed.
    fn drop_key(&mut self, parent: &str, key: &str, reason: DropReason) {
        self.dropped.push(Dropped {
            pointer: child(parent, key),
            reason,
        });
        self.removals.push((parent.to_owned(), key.to_owned()));
    }

    /// Walk the whole config. Top-level keys come from an allowlist (ADR
    /// 0006); the output never has `services`.
    pub fn root(mut self, root: &Map<String, Value>) -> Self {
        for (key, value) in root {
            let at = child("", key);
            if !is_canonical(key) {
                self.refuse(at, RefusalKind::NonCanonicalKey);
                continue;
            }
            match key.as_str() {
                // An editor annotation sing-box ignores; the helper's config
                // holds only what was checked.
                "$schema" => self.drop_key("", key, DropReason::Annotation),
                // BoxPilot owns inbounds; the helper adds its own.
                "inbounds" => self.refuse(at, RefusalKind::Inbounds),
                // Removed whole once its entries are judged: what isn't
                // dropped is refused.
                "services" => {
                    self.services(&at, value);
                    self.removals.push((String::new(), key.clone()));
                }
                "experimental" => self.experimental(&at, value),
                "log" => self.visit(&at, value, Node::Object(Scope::Log), 2),
                "dns" => self.visit(&at, value, Node::Object(Scope::Dns), 2),
                "ntp" => self.visit(&at, value, Node::Object(Scope::Ntp), 2),
                "certificate" => self.visit(&at, value, Node::Object(Scope::Certificate), 2),
                "route" => self.visit(&at, value, Node::Object(Scope::Route), 2),
                "outbounds" => self.visit(&at, value, Node::ArrayOf(Element::Outbound), 2),
                "endpoints" => self.visit(&at, value, Node::ArrayOf(Element::Endpoint), 2),
                // `certificate_providers`, `http_clients`,
                // `network_namespaces`, and whatever comes next.
                _ => self.refuse(at, RefusalKind::UnknownSection),
            }
        }
        self
    }

    /// `services`: the profile's own `api` services are control planes,
    /// dropped (only the helper's runs); every other service is refused.
    fn services(&mut self, at: &str, value: &Value) {
        let Value::Array(services) = value else {
            self.refuse(
                at.to_owned(),
                RefusalKind::Malformed {
                    expected: Expected::Array,
                },
            );
            return;
        };
        for (i, service) in services.iter().enumerate() {
            let at = child(at, &i.to_string());
            let Value::Object(service) = service else {
                self.refuse(
                    at,
                    RefusalKind::Malformed {
                        expected: Expected::Object,
                    },
                );
                continue;
            };
            if let Some(key) = service.keys().find(|key| !is_canonical(key)) {
                self.refuse(child(&at, key), RefusalKind::NonCanonicalKey);
                continue;
            }
            match service.get("type") {
                Some(Value::String(kind)) if kind == "api" => self.dropped.push(Dropped {
                    pointer: at,
                    reason: DropReason::ControlPlane,
                }),
                Some(Value::String(kind)) => self.refuse(
                    child(&at, "type"),
                    RefusalKind::Service {
                        service_type: Some(kind.clone()),
                    },
                ),
                _ => self.refuse(at, RefusalKind::Service { service_type: None }),
            }
        }
    }

    /// `experimental`: `cache_file` stays (its path is the helper's),
    /// `clash_api` and `v2ray_api` are control planes, dropped; anything else
    /// (`debug`, with its pprof listener, and whatever comes next) is
    /// refused.
    fn experimental(&mut self, at: &str, value: &Value) {
        let Value::Object(experimental) = value else {
            self.refuse(
                at.to_owned(),
                RefusalKind::Malformed {
                    expected: Expected::Object,
                },
            );
            return;
        };
        for (key, value) in experimental {
            let key_at = child(at, key);
            if !is_canonical(key) {
                self.refuse(key_at, RefusalKind::NonCanonicalKey);
                continue;
            }
            match (key.as_str(), value) {
                ("clash_api" | "v2ray_api", _) => self.drop_key(at, key, DropReason::ControlPlane),
                // An object, though its name ends in `_file`. As a string
                // it would be a path, which is what the shape rule is for.
                ("cache_file", Value::Object(cache_file)) => {
                    self.object(&key_at, cache_file, Scope::CacheFile, 3)
                }
                ("cache_file", Value::String(_)) => {
                    self.refuse(key_at, RefusalKind::FilesystemPath)
                }
                ("cache_file", _) => self.refuse(
                    key_at,
                    RefusalKind::Malformed {
                        expected: Expected::Object,
                    },
                ),
                _ => self.refuse(key_at, RefusalKind::UnknownExperimental),
            }
        }
    }

    /// Walk `value`, which sits `depth` levels deep, as `node` says.
    fn visit(&mut self, at: &str, value: &Value, node: Node, depth: usize) {
        if depth > WALK_DEPTH_LIMIT {
            self.refuse(
                at.to_owned(),
                RefusalKind::TooDeep {
                    limit: WALK_DEPTH_LIMIT,
                },
            );
            return;
        }
        let expected = match (node, value) {
            (Node::Plain, value) => return self.plain(at, value, Scope::Generic, depth),
            (Node::PlainOr(scope), value) => return self.plain(at, value, scope, depth),
            (Node::Object(scope), Value::Object(object)) => {
                return self.object(at, object, scope, depth)
            }
            (Node::Transport, Value::Object(object)) => {
                let scope = Scope::Transport(TransportKind::of(object));
                return self.object(at, object, scope, depth);
            }
            (Node::Map, Value::Object(map)) => return self.map(at, map),
            (Node::ArrayOf(element), Value::Array(items)) => {
                for (i, item) in items.iter().enumerate() {
                    let at = child(at, &i.to_string());
                    match item {
                        Value::Object(object) => self.element(&at, object, element, depth + 1),
                        _ => self.refuse(
                            at,
                            RefusalKind::Malformed {
                                expected: Expected::Object,
                            },
                        ),
                    }
                }
                return;
            }
            (Node::ArrayOf(_), _) => Expected::Array,
            (Node::Object(_) | Node::Transport | Node::Map, _) => Expected::Object,
        };
        self.refuse(at.to_owned(), RefusalKind::Malformed { expected });
    }

    /// A value with no rules of its own: objects get `scope`, arrays are
    /// walked element by element, scalars are fine.
    fn plain(&mut self, at: &str, value: &Value, scope: Scope, depth: usize) {
        match value {
            Value::Object(object) => self.object(at, object, scope, depth),
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    self.visit(&child(at, &i.to_string()), item, Node::Plain, depth + 1);
                }
            }
            _ => {}
        }
    }

    /// One element of an `ArrayOf`, its scope read from its `type`.
    fn element(&mut self, at: &str, object: &Map<String, Value>, element: Element, depth: usize) {
        if depth > WALK_DEPTH_LIMIT {
            self.refuse(
                at.to_owned(),
                RefusalKind::TooDeep {
                    limit: WALK_DEPTH_LIMIT,
                },
            );
            return;
        }
        let scope = match element {
            Element::Outbound => Scope::Outbound(OutboundKind::of(object)),
            Element::Endpoint => {
                let kind = EndpointKind::of(object);
                self.endpoint(at, object, kind);
                Scope::Endpoint(kind)
            }
            Element::DnsServer => Scope::DnsServer(DnsServerKind::of(object)),
            Element::Rule => Scope::Rule,
            Element::RuleSet => Scope::RuleSet(RuleSetKind::of(object)),
            Element::TnccCertificate => Scope::TnccCertificate,
        };
        self.object(at, object, scope, depth);
    }

    /// What an endpoint is as a whole, beyond its keys.
    fn endpoint(&mut self, at: &str, object: &Map<String, Value>, kind: EndpointKind) {
        match kind {
            // `materialize` gives it state directories by the tag sing-box
            // runs it under: its own, or its index in `endpoints`
            // (`box.go`).
            EndpointKind::Tailscale => {
                let tag = match object.get("tag").and_then(Value::as_str) {
                    Some(tag) if !tag.is_empty() => tag.to_owned(),
                    _ => at.rsplit('/').next().unwrap_or_default().to_owned(),
                };
                self.tailscale.push(TailscaleEndpoint {
                    pointer: at.to_owned(),
                    tag,
                });
            }
            // The AnyConnect flavor (the default) runs a built-in CSD host
            // scan whenever the server asks for one: it stats each file the
            // server names and reports its timestamps and CRC32
            // (sing-openconnect `anyconnect_csd.go`). As root that is a probe
            // of files the owner can't read. No option turns it off.
            EndpointKind::OpenConnect => {
                let anyconnect = match object.get("flavor") {
                    None | Some(Value::Null) => true,
                    Some(Value::String(flavor)) => flavor.is_empty() || flavor == "anyconnect",
                    Some(_) => true,
                };
                if anyconnect {
                    let key = if object.contains_key("flavor") {
                        "flavor"
                    } else {
                        "type"
                    };
                    self.refuse(child(at, key), RefusalKind::ServerFileScan);
                }
            }
            _ => {}
        }
    }

    /// An object of `scope`, `depth` levels deep: each key by its rule.
    fn object(&mut self, at: &str, object: &Map<String, Value>, scope: Scope, depth: usize) {
        for (key, value) in object {
            if !is_canonical(key) {
                self.refuse(child(at, key), RefusalKind::NonCanonicalKey);
                continue;
            }
            match action(scope, key, value) {
                Action::Drop(reason) => self.drop_key(at, key, reason),
                Action::Refuse(kind) => self.refuse(child(at, key), kind),
                Action::Read(arity) => self.read(child(at, key), value, arity),
                Action::Visit(node) => self.visit(&child(at, key), value, node, depth + 1),
            }
        }
    }

    /// A map whose keys are data. Its values are strings or lists of them.
    fn map(&mut self, at: &str, map: &Map<String, Value>) {
        for (key, value) in map {
            let nested = match value {
                Value::Object(_) => true,
                Value::Array(items) => items.iter().any(|item| item.is_object() || item.is_array()),
                _ => false,
            };
            if nested {
                self.refuse(
                    child(at, key),
                    RefusalKind::Malformed {
                        expected: Expected::StringOrArray,
                    },
                );
            }
        }
    }

    /// A file-read field: each path in it must be a reference to an
    /// attachment the request carries.
    fn read(&mut self, at: String, value: &Value, arity: Arity) {
        match (value, arity) {
            // Unset.
            (Value::Null, _) => {}
            (Value::String(path), _) => self.read_one(at, path),
            (Value::Array(paths), Arity::Many) => {
                for (i, path) in paths.iter().enumerate() {
                    let at = child(&at, &i.to_string());
                    match path {
                        Value::String(path) => self.read_one(at, path),
                        _ => self.refuse(
                            at,
                            RefusalKind::Malformed {
                                expected: Expected::String,
                            },
                        ),
                    }
                }
            }
            (_, Arity::One) => self.refuse(
                at,
                RefusalKind::Malformed {
                    expected: Expected::String,
                },
            ),
            (_, Arity::Many) => self.refuse(
                at,
                RefusalKind::Malformed {
                    expected: Expected::StringOrArray,
                },
            ),
        }
    }

    fn read_one(&mut self, at: String, path: &str) {
        // An empty path names no file: sing-box reads one only when it is
        // set.
        if path.is_empty() {
            return;
        }
        match path.strip_prefix(ATTACHMENT_PREFIX) {
            Some(id) if !is_attachment_id(id) => {
                self.refuse(at, RefusalKind::MalformedAttachment);
            }
            Some(id) if !self.attachments.contains(id) => {
                self.refuse(at, RefusalKind::MissingAttachment { id: id.to_owned() })
            }
            Some(id) => self.refs.push(AttachmentRef {
                pointer: at,
                id: id.to_owned(),
            }),
            None => {
                self.local_files.push(LocalFileField {
                    pointer: at.clone(),
                    path: path.to_owned(),
                });
                self.refuse(at, RefusalKind::LocalFile);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nesting_counts_brackets_outside_strings() {
        // Object, array, object, array: four levels.
        assert!(!nesting_exceeds(r#"{"a": [1, {"b": []}]}"#, 4));
        assert!(nesting_exceeds(r#"{"a": [1, {"b": []}]}"#, 3));
        // Brackets in strings, escaped quotes included, don't nest.
        assert!(!nesting_exceeds(r#"{"a": "[[[{{{\"[[["}"#, 1));
        assert!(!nesting_exceeds("", 0));
        assert!(nesting_exceeds("[", 0));
    }

    /// Upper-case ASCII and every non-ASCII character make a key
    /// non-canonical: the two Go folds to ASCII letters, and the rest too,
    /// so the rule doesn't hang on Go's folding table.
    #[test]
    fn canonical_keys_are_lower_case_ascii() {
        for key in ["path", "$schema", "tcp_multi_path", "geosite-cn", "x_1"] {
            assert!(is_canonical(key), "{key}");
        }
        for key in [
            "Path",
            "TYPE",
            "\u{17F}tate_directory",
            "\u{212A}ey_path",
            "ключ",
            "pat\u{0127}",
        ] {
            assert!(!is_canonical(key), "{key}");
        }
    }

    #[test]
    fn path_shape() {
        for key in [
            "path",
            "directory",
            "output",
            "pid_file",
            "key_path",
            "data_directory",
            "cache_file",
            "dhcp_lease_files",
            "certificate_paths",
            "work_dir",
            "debug_output",
        ] {
            assert!(is_path_shaped(key), "{key}");
        }
        for key in [
            "process_path_regex",
            "disable_path_mtu_discovery",
            "direction",
            "key_direction",
            "profile",
            "url",
            "pathname",
        ] {
            assert!(!is_path_shaped(key), "{key}");
        }
    }

    fn keys(opts: &str) -> Option<Vec<String>> {
        plugin_option_keys(opts).map(|keys| {
            keys.into_iter()
                .map(|key| String::from_utf8(key).unwrap())
                .collect()
        })
    }

    /// Parsed as sing-box's `ParsePluginOptions` parses them, escapes and
    /// valueless keys included.
    #[test]
    fn plugin_options_parse_like_sip003() {
        assert_eq!(keys(""), Some(vec![]));
        assert_eq!(
            keys("tls;host=cdn.example.com;path=/ws;mux=4"),
            Some(vec![
                "tls".into(),
                "host".into(),
                "path".into(),
                "mux".into()
            ])
        );
        assert_eq!(keys(r"c\ert=/etc/ssl/ca.pem"), Some(vec!["cert".into()]));
        assert_eq!(keys(r"host=a\;cert=x"), Some(vec!["host".into()]));
        assert_eq!(
            keys("obfs=http;obfs-host=a.example;"),
            Some(vec!["obfs".into(), "obfs-host".into()])
        );
        // Empty key; trailing lone escape.
        assert_eq!(keys("=x"), None);
        assert_eq!(keys(r"host=a\"), None);
    }

    #[test]
    fn pointers_escape_tilde_and_slash() {
        assert_eq!(child("", "a/b~c"), "/a~1b~0c");
        assert_eq!(child("/outbounds", "0"), "/outbounds/0");
        assert_eq!(child("", ""), "/");
    }
}
