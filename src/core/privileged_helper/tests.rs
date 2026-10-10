//! The pure half of the helper client. Messages are checked in English
//! (`s()` without `set_language`, as every core test does), and in each
//! platform's terms by name (`HelperOs`), so every OS checks both.

use super::*;
use crate::i18n::EN;
use serde_json::json;
use std::cell::RefCell;

fn options() -> TunOptions {
    TunOptions {
        ipv6: true,
        proxy_port: 7788,
        allow_lan: false,
        system_proxy: true,
    }
}

fn refusal(pointer: &str, code: RefusalCode, detail: Option<&str>) -> WireRefusal {
    WireRefusal {
        pointer: pointer.to_string(),
        code,
        detail: detail.map(str::to_string),
    }
}

// ---- Which sing-box a start runs ----

/// The decision table: only an unelevated TUN start on the helper's
/// platform goes through the helper.
#[test]
fn only_unelevated_tun_on_the_helper_platform_uses_the_helper() {
    for (platform, proxy_mode, elevated, route) in [
        (true, false, false, StartRoute::Helper),
        (true, false, true, StartRoute::Local),
        (true, true, false, StartRoute::Local),
        (true, true, true, StartRoute::Local),
        (false, false, false, StartRoute::Local),
        (false, false, true, StartRoute::Local),
        (false, true, false, StartRoute::Local),
    ] {
        assert_eq!(
            start_route(platform, proxy_mode, elevated),
            route,
            "platform {platform}, proxy {proxy_mode}, elevated {elevated}"
        );
    }
    assert_eq!(
        HELPER_PLATFORM,
        cfg!(any(target_os = "windows", target_os = "macos"))
    );
    assert_eq!(HELPER_INSTALLED_BY_APP, cfg!(target_os = "macos"));
    assert_eq!(GUI_SETS_SYSTEM_PROXY, cfg!(target_os = "windows"));
    assert_eq!(
        HelperOs::CURRENT == HelperOs::MacOs,
        cfg!(target_os = "macos")
    );
}

#[test]
fn tun_options_come_from_the_settings() {
    let settings = AppSettings {
        tun_ipv6: true,
        proxy_port: 18888,
        allow_lan: true,
        set_system_proxy: true,
        ..AppSettings::default()
    };
    assert_eq!(
        tun_options(&settings),
        TunOptions {
            ipv6: true,
            proxy_port: 18888,
            allow_lan: true,
            system_proxy: true,
        }
    );
}

// ---- Preparing a start ----

/// A profile that reads three local files, one of them twice.
fn profile_with_files() -> String {
    json!({
        "certificate": {"certificate_path": ["C:\\certs\\root.pem"]},
        "outbounds": [
            {"type": "trojan", "tag": "t", "server": "t.example", "server_port": 443,
             "password": "p", "tls": {"enabled": true, "certificate_path": "C:\\certs\\root.pem",
             "client_key_path": "C:\\certs\\me.key"}},
            {"type": "direct", "tag": "direct"}
        ],
        "route": {"rule_set": [
            {"type": "local", "tag": "cn", "format": "binary", "path": "rules/cn.srs"}
        ]}
    })
    .to_string()
}

#[test]
fn local_files_travel_as_attachments_read_once_each() {
    let reads = RefCell::new(Vec::new());
    let prepared = prepare_start(&profile_with_files(), options(), |path, limit| {
        assert!(limit > 0);
        reads.borrow_mut().push(path.to_string());
        Ok(format!("content of {path}").into_bytes())
    })
    .unwrap();

    // Each distinct path once, as the config writes it (relative too: the
    // caller resolves it as sing-box would).
    let mut read = reads.into_inner();
    read.sort();
    assert_eq!(
        read,
        ["C:\\certs\\me.key", "C:\\certs\\root.pem", "rules/cn.srs"]
    );

    let request = &prepared.request;
    assert_eq!(request.options, options());
    assert_eq!(request.attachments.len(), 3);
    let config: Value = serde_json::from_str(&request.config).unwrap();
    let by_id = |id: &str| -> String {
        let data = &request.attachments.iter().find(|(i, _)| i == id).unwrap().1;
        String::from_utf8(data.clone()).unwrap()
    };
    let reference = |value: &Value| -> String {
        let text = value.as_str().unwrap();
        text.strip_prefix(boxpilot_policy::ATTACHMENT_PREFIX)
            .unwrap_or_else(|| panic!("{text} is not a reference"))
            .to_string()
    };
    // The same file twice is the same attachment.
    let root_ca = reference(&config["certificate"]["certificate_path"][0]);
    assert_eq!(
        reference(&config["outbounds"][0]["tls"]["certificate_path"]),
        root_ca
    );
    assert_eq!(by_id(&root_ca), "content of C:\\certs\\root.pem");
    let key = reference(&config["outbounds"][0]["tls"]["client_key_path"]);
    assert_eq!(by_id(&key), "content of C:\\certs\\me.key");
    let rules = reference(&config["route"]["rule_set"][0]["path"]);
    assert_eq!(by_id(&rules), "content of rules/cn.srs");
    // No path reaches the helper.
    assert!(!request.config.contains("certs"));
    assert!(!request.config.contains("cn.srs"));
    for (id, _) in &request.attachments {
        assert!(boxpilot_policy::is_attachment_id(id), "{id}");
    }
}

#[test]
fn a_profile_without_local_files_reads_nothing() {
    let config = json!({"outbounds": [{"type": "direct", "tag": "direct"}]}).to_string();
    let prepared = prepare_start(&config, options(), |path, _| {
        panic!("read {path}");
    })
    .unwrap();
    assert!(prepared.request.attachments.is_empty());
}

/// The running view carries BoxPilot's inbounds, never the helper's `api`
/// service (its secret stays off disk) nor a system proxy sing-box would
/// write, and the config's own control planes are gone as on the helper.
#[test]
fn the_running_view_is_what_the_helper_runs_minus_its_own_parts() {
    let config = json!({
        "outbounds": [{"type": "direct", "tag": "direct"}],
        "experimental": {"clash_api": {"external_controller": "0.0.0.0:9090"}},
        "services": [{"type": "api", "tag": "theirs", "listen": "0.0.0.0", "listen_port": 9091}]
    })
    .to_string();
    let prepared = prepare_start(&config, options(), |_, _| unreachable!()).unwrap();
    let view: Value = serde_json::from_str(&prepared.running_view).unwrap();
    let inbounds = view["inbounds"].as_array().unwrap();
    assert_eq!(inbounds[0]["type"], "tun");
    assert_eq!(
        inbounds[0]["address"].as_array().unwrap().len(),
        2,
        "IPv6 on"
    );
    assert_eq!(inbounds[1]["type"], "mixed");
    assert_eq!(inbounds[1]["listen_port"], 7788);
    assert!(inbounds[1].get("set_system_proxy").is_none());
    assert!(view.get("services").is_none());
    assert!(view["experimental"].get("clash_api").is_none());
    assert_eq!(view["experimental"]["cache_file"]["enabled"], true);
    assert!(!prepared.running_view.contains("secret"));
    // The helper's own rule comes first, as in what it runs.
    assert_eq!(
        view["route"]["rules"][0],
        boxpilot_runconfig::loopback_rule()
    );
}

/// The preview while stopped is the running view a start would get, or why
/// that start wouldn't reach the helper.
#[test]
fn a_preview_is_the_running_view_a_start_would_get() {
    let config = json!({
        "outbounds": [{"type": "direct", "tag": "direct"}],
        "route": {"rules": [{"domain": ["example.com"], "outbound": "direct"}]}
    })
    .to_string();
    let dir = std::env::temp_dir();
    let preview = preview_start(&config, &dir, options()).unwrap();
    assert_eq!(
        preview,
        prepare_start(&config, options(), |_, _| unreachable!())
            .unwrap()
            .running_view
    );
    let view: Value = serde_json::from_str(&preview).unwrap();
    assert_eq!(
        view["route"]["rules"][0],
        boxpilot_runconfig::loopback_rule()
    );
    assert_eq!(view["route"]["rules"][1]["domain"][0], "example.com");

    let refused = json!({"outbounds": [{"type": "tor", "tag": "tor"}]}).to_string();
    assert_eq!(
        preview_start(&refused, &dir, options()).unwrap_err(),
        "The privileged helper won't run this profile in TUN mode: \
         /outbounds/0/type runs a program. Proxy mode runs it as written."
    );
}

/// A stream that only counts its closes.
#[derive(Default)]
struct Closes(std::sync::atomic::AtomicUsize);

impl HelperIo for Closes {
    fn read(&self, _: &mut [u8]) -> io::Result<usize> {
        Ok(0)
    }

    fn write_all(&self, _: &[u8], _: Instant) -> io::Result<()> {
        Ok(())
    }

    fn close(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Closes {
    fn closes(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// A stop ends a helper start's connection whenever it comes: before the
/// connection opens (it is closed as soon as it does), or while the start
/// waits on it. It never keeps the connection alive itself.
#[test]
fn a_cancel_closes_the_start_connection_whenever_it_comes() {
    let early = StartCancel::default();
    early.cancel();
    let io = Arc::new(Closes::default());
    let shared: Arc<dyn HelperIo> = io.clone();
    assert!(!early.attach(&shared));
    assert_eq!(io.closes(), 1);

    let during = StartCancel::default();
    let io = Arc::new(Closes::default());
    let shared: Arc<dyn HelperIo> = io.clone();
    assert!(during.attach(&shared));
    assert_eq!(io.closes(), 0);
    during.clone().cancel();
    assert_eq!(io.closes(), 1);

    let after = StartCancel::default();
    let shared: Arc<dyn HelperIo> = Arc::new(Closes::default());
    assert!(after.attach(&shared));
    let weak = Arc::downgrade(&shared);
    drop(shared);
    assert!(weak.upgrade().is_none(), "the cancel holds the connection");
    after.cancel();
}

#[test]
fn a_refused_profile_never_reaches_the_helper() {
    let config = json!({"outbounds": [
        {"type": "direct", "tag": "d"},
        {"type": "tor", "tag": "tor"}
    ]})
    .to_string();
    let error = match prepare_start(&config, options(), |_, _| unreachable!()) {
        Err(error) => error,
        Ok(_) => panic!("tor passed"),
    };
    assert_eq!(
        error,
        PrepareError::Refused(vec![refusal(
            "/outbounds/1/type",
            RefusalCode::RunsProgram,
            None
        )])
    );
    assert_eq!(
        error.message(),
        "The privileged helper won't run this profile in TUN mode: \
         /outbounds/1/type runs a program. Proxy mode runs it as written."
    );
}

#[test]
fn a_config_that_is_not_json_is_refused_whole() {
    let error = match prepare_start("{not json", options(), |_, _| unreachable!()) {
        Err(error) => error,
        Ok(_) => panic!("passed"),
    };
    let PrepareError::Refused(refusals) = &error else {
        panic!("{error:?}");
    };
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0].pointer, "");
    assert_eq!(refusals[0].code, RefusalCode::InvalidJson);
    assert!(error.message().contains(": the config is not valid JSON ("));
}

#[test]
fn an_unreadable_file_names_its_path_and_field() {
    let error = match prepare_start(&profile_with_files(), options(), |path, _| {
        if path == "rules/cn.srs" {
            Err(io::Error::new(io::ErrorKind::NotFound, "no such file"))
        } else {
            Ok(Vec::new())
        }
    }) {
        Err(error) => error,
        Ok(_) => panic!("passed"),
    };
    assert_eq!(
        error,
        PrepareError::ReadFile {
            path: "rules/cn.srs".into(),
            pointer: "/route/rule_set/0/path".into(),
            error: "no such file".into(),
        }
    );
    assert_eq!(
        error.message(),
        "TUN mode can't read rules/cn.srs (used by /route/rule_set/0/path): no such file"
    );
}

/// The reader is told how much room is left, and more than that is too
/// large: one `start` carries 32 MiB in all.
#[test]
fn files_beyond_what_one_start_carries_are_too_large() {
    let limit = boxpilot_protocol::Limits::default().max_start_total;
    let error = match prepare_start(&profile_with_files(), options(), |_, room| {
        assert!(room < limit);
        Ok(vec![0; room + 1])
    }) {
        Err(error) => error,
        Ok(_) => panic!("passed"),
    };
    assert!(
        matches!(error, PrepareError::TooLarge { bytes, limit: l } if bytes > limit && l == limit),
        "{error:?}"
    );
}

#[test]
fn more_distinct_files_than_one_start_carries_are_refused() {
    let max = boxpilot_protocol::Limits::default().max_attachments;
    let rule_sets: Vec<Value> = (0..=max)
        .map(|n| json!({"type": "local", "tag": format!("r{n}"), "format": "binary", "path": format!("r{n}.srs")}))
        .collect();
    let config = json!({"route": {"rule_set": rule_sets}}).to_string();
    let error = match prepare_start(&config, options(), |_, _| Ok(vec![1])) {
        Err(error) => error,
        Ok(_) => panic!("passed"),
    };
    assert_eq!(
        error,
        PrepareError::TooManyFiles {
            count: max + 1,
            limit: max
        }
    );
}

// ---- Reaching the helper ----

#[test]
fn win32_codes_map_to_what_the_plan_tells_apart() {
    assert_eq!(PipeOpen::from_win32(2), PipeOpen::NotFound);
    assert_eq!(PipeOpen::from_win32(231), PipeOpen::Busy);
    assert_eq!(PipeOpen::from_win32(5), PipeOpen::Denied);
    assert_eq!(PipeOpen::from_win32(87), PipeOpen::Other(87));

    assert_eq!(ServiceStart::from_win32(None), ServiceStart::Started);
    assert_eq!(
        ServiceStart::from_win32(Some(1056)),
        ServiceStart::AlreadyRunning
    );
    assert_eq!(
        ServiceStart::from_win32(Some(1060)),
        ServiceStart::NotInstalled
    );
    assert_eq!(
        ServiceStart::from_win32(Some(1072)),
        ServiceStart::NotInstalled
    );
    assert_eq!(ServiceStart::from_win32(Some(1058)), ServiceStart::Disabled);
    assert_eq!(ServiceStart::from_win32(Some(5)), ServiceStart::Denied);
    assert_eq!(
        ServiceStart::from_win32(Some(1053)),
        ServiceStart::Failed(1053)
    );
}

#[test]
fn a_missing_pipe_starts_the_service_and_backs_off() {
    let now = Instant::now();
    let mut plan = ConnectPlan::new(now);
    assert_eq!(
        plan.pipe_failed(PipeOpen::NotFound, now),
        Ok(ConnectStep::StartService)
    );
    assert_eq!(plan.service_started(ServiceStart::Started), Ok(()));
    let waits: Vec<Duration> = (0..6).map(|_| plan.backoff(now).unwrap()).collect();
    assert_eq!(
        waits,
        [50, 100, 200, 400, 500, 500].map(Duration::from_millis)
    );
    // Started by this connect: a missing pipe now means "not listening
    // yet" or "failed", which its status tells apart.
    assert_eq!(
        plan.pipe_failed(PipeOpen::NotFound, now),
        Ok(ConnectStep::CheckService)
    );
    assert_eq!(plan.service_state(ServiceState::Active), Ok(false));
    // A clean stop is an idle exit racing the start: start it again.
    assert_eq!(
        plan.service_state(ServiceState::Stopped {
            win32_exit: 0,
            service_exit: 0,
        }),
        Ok(true)
    );
    assert_eq!(
        plan.pipe_failed(PipeOpen::NotFound, now),
        Ok(ConnectStep::StartService)
    );
    // Already running (it may be stopping after its idle exit): wait it
    // out and start it once it has stopped.
    let mut fresh = ConnectPlan::new(now);
    assert_eq!(fresh.service_started(ServiceStart::AlreadyRunning), Ok(()));
    assert_eq!(
        fresh.pipe_failed(PipeOpen::NotFound, now),
        Ok(ConnectStep::StartService)
    );
}

#[test]
fn a_service_that_stops_with_an_exit_code_says_why() {
    let now = Instant::now();
    let mut plan = ConnectPlan::new(now);
    plan.service_started(ServiceStart::Started).unwrap();
    assert_eq!(
        plan.service_state(ServiceState::Stopped {
            win32_exit: win32::ERROR_SERVICE_SPECIFIC_ERROR,
            service_exit: exit::MANIFEST_REFUSED as u32,
        }),
        Err(OpenError::ServiceExited(exit::MANIFEST_REFUSED))
    );
    assert_eq!(
        plan.service_state(ServiceState::Stopped {
            win32_exit: 1053,
            service_exit: 0,
        }),
        Err(OpenError::ServiceFailed(1053))
    );
}

#[test]
fn service_start_failures_end_the_connect() {
    let mut plan = ConnectPlan::new(Instant::now());
    assert_eq!(
        plan.service_started(ServiceStart::NotInstalled),
        Err(OpenError::NotInstalled)
    );
    assert_eq!(
        plan.service_started(ServiceStart::Disabled),
        Err(OpenError::Disabled)
    );
    assert_eq!(
        plan.service_started(ServiceStart::Denied),
        Err(OpenError::StartDenied)
    );
    assert_eq!(
        plan.service_started(ServiceStart::Failed(1053)),
        Err(OpenError::ServiceFailed(1053))
    );
}

#[test]
fn a_busy_or_denied_pipe_and_the_deadline() {
    let now = Instant::now();
    let mut plan = ConnectPlan::new(now);
    assert_eq!(
        plan.pipe_failed(PipeOpen::Busy, now),
        Ok(ConnectStep::WaitBusy(Duration::from_secs(2)))
    );
    // Near the deadline the busy wait shrinks to what is left.
    let late = now + CONNECT_TIMEOUT - Duration::from_millis(300);
    assert_eq!(
        plan.pipe_failed(PipeOpen::Busy, late),
        Ok(ConnectStep::WaitBusy(Duration::from_millis(300)))
    );
    assert_eq!(plan.backoff(late), Ok(Duration::from_millis(50)));
    assert_eq!(
        plan.pipe_failed(PipeOpen::Denied, now),
        Err(OpenError::ConnectDenied)
    );
    let past = now + CONNECT_TIMEOUT;
    assert_eq!(
        plan.pipe_failed(PipeOpen::NotFound, past),
        Err(OpenError::TimedOut)
    );
    assert_eq!(plan.backoff(past), Err(OpenError::TimedOut));
}

/// What a failed connect to the macOS helper's socket means: nobody there
/// is "not installed" or "turned off", by its plist.
#[test]
fn socket_connect_errors_map_to_what_the_user_can_do() {
    let error = |kind| io::Error::from(kind);
    for kind in [io::ErrorKind::NotFound, io::ErrorKind::ConnectionRefused] {
        assert_eq!(
            socket_connect_error(&error(kind), || false),
            OpenError::NotInstalled
        );
        assert_eq!(
            socket_connect_error(&error(kind), || true),
            OpenError::Disabled
        );
    }
    assert_eq!(
        socket_connect_error(&error(io::ErrorKind::PermissionDenied), || {
            panic!("the plist doesn't matter")
        }),
        OpenError::ConnectDenied
    );
    assert!(matches!(
        socket_connect_error(&io::Error::other("boom"), || true),
        OpenError::Os(message) if message == "boom"
    ));
}

// ---- What the user reads ----

/// Every refusal code has its own words, the pointer first (or "the
/// config" for the whole of it), and an unknown code still reads.
#[test]
fn every_refusal_code_reads() {
    let cases = [
        (refusal("", RefusalCode::TooLarge, Some("40 > 32")), "the config is too large (40 > 32 bytes)"),
        (refusal("", RefusalCode::TooDeep, Some("64")), "the config nests deeper than 64 levels"),
        (refusal("", RefusalCode::InvalidJson, Some("EOF at line 1")), "the config is not valid JSON (EOF at line 1)"),
        (refusal("", RefusalCode::NotAnObject, None), "the config is not a JSON object"),
        (refusal("/outbounds", RefusalCode::Malformed, Some("array")), "/outbounds is not an array"),
        (refusal("/dns", RefusalCode::Malformed, Some("object")), "/dns is not an object"),
        (refusal("/a", RefusalCode::Malformed, Some("string")), "/a is not a string"),
        (refusal("/a", RefusalCode::Malformed, Some("string_or_array")), "/a is not a string or an array of strings"),
        (refusal("/a", RefusalCode::Malformed, Some("plugin_options")), "/a is not valid SIP003 plugin options"),
        (refusal("/Log", RefusalCode::NonCanonicalKey, None), "/Log is not spelled in lower case; sing-box reads field names case-insensitively, so it can't be checked"),
        (refusal("/foo", RefusalCode::UnknownSection, None), "/foo is not a section the privileged helper runs"),
        (refusal("/outbounds/0/type", RefusalCode::TypeNotAllowed, Some("bridge")), "/outbounds/0/type is type \"bridge\", which the privileged helper doesn't run"),
        (refusal("/outbounds/0", RefusalCode::TypeNotAllowed, None), "/outbounds/0 has no type the privileged helper runs"),
        (refusal("/inbounds", RefusalCode::Inbounds, None), "/inbounds defines inbounds; BoxPilot adds its own"),
        (refusal("/services/0", RefusalCode::Service, Some("derp")), "/services/0 runs a \"derp\" service, which the privileged helper doesn't allow"),
        (refusal("/services/1", RefusalCode::Service, None), "/services/1 runs a service, which the privileged helper doesn't allow"),
        (refusal("/experimental/debug", RefusalCode::UnknownExperimental, None), "/experimental/debug is an experimental option the privileged helper doesn't allow"),
        (refusal("/outbounds/1/type", RefusalCode::RunsProgram, None), "/outbounds/1/type runs a program"),
        (refusal("/ntp/write_to_system", RefusalCode::SystemChange, None), "/ntp/write_to_system changes the system beyond networking"),
        (refusal("/endpoints/0/flavor", RefusalCode::ServerFileScan, None), "/endpoints/0/flavor lets the VPN server inspect local files (the AnyConnect host scan)"),
        (refusal("/log/output", RefusalCode::FilesystemPath, None), "/log/output names a file or folder on this computer"),
        (refusal("/certificate/certificate_directory_path", RefusalCode::Directory, None), "/certificate/certificate_directory_path names a folder to read, which can't be sent to the privileged helper"),
        (refusal("/a/key_path", RefusalCode::LocalFile, None), "/a/key_path reads a local file that wasn't sent along"),
        (refusal("/a/key_path", RefusalCode::MalformedAttachment, None), "/a/key_path is not a valid attachment reference"),
        (refusal("/a/key_path", RefusalCode::MissingAttachment, Some("file-3")), "/a/key_path refers to attachment \"file-3\", which wasn't sent"),
        (refusal("/a", RefusalCode::Other("from_the_future".into()), None), "/a was refused (from_the_future)"),
    ];
    for (refusal, text) in cases {
        assert_eq!(refusal_text(&refusal), text, "{:?}", refusal.code);
    }
}

#[test]
fn a_long_refusal_list_counts_the_rest() {
    let refusals: Vec<WireRefusal> = (0..5)
        .map(|n| {
            refusal(
                &format!("/outbounds/{n}/type"),
                RefusalCode::RunsProgram,
                None,
            )
        })
        .collect();
    assert_eq!(
        refused_message(&refusals, 2),
        "The privileged helper won't run this profile in TUN mode: \
         /outbounds/0/type runs a program; /outbounds/1/type runs a program; \
         /outbounds/2/type runs a program; and 4 more. Proxy mode runs it as written."
    );
    assert_eq!(
        refused_message(&refusals[..1], 0),
        "The privileged helper won't run this profile in TUN mode: \
         /outbounds/0/type runs a program. Proxy mode runs it as written."
    );
}

#[test]
fn chinese_refusals_use_full_width_punctuation() {
    let h = &crate::i18n::ZH_CN.helper;
    assert_eq!(
        (h.refusal_at)("/outbounds/1/type", h.runs_program),
        "/outbounds/1/type：会运行程序"
    );
    assert_eq!(
        (h.refused)(&[(h.refusal_at)(h.whole_config, h.not_an_object), (h.refused_more)(2)].join(h.refusal_sep)),
        "特权助手不会以 TUN 模式运行此配置：配置文件：不是 JSON 对象；另有 2 项。代理模式可以照原样运行它。"
    );
}

#[test]
fn every_error_code_reads() {
    let windows = |code, message| error_message_on(code, message, HelperOs::Windows);
    assert_eq!(windows(ErrorCode::Unauthorized, "x"), EN.helper.not_allowed);
    assert_eq!(
        windows(ErrorCode::VersionMismatch, "x"),
        EN.helper.version_mismatch
    );
    assert_eq!(windows(ErrorCode::Busy, "x"), EN.helper.busy);
    assert_eq!(
        windows(ErrorCode::BadRequest, "`config_len` is 0"),
        "The privileged helper didn't accept BoxPilot's request: `config_len` is 0"
    );
    assert_eq!(
        windows(
            ErrorCode::Internal,
            "the run directory could not be written"
        ),
        "The privileged helper couldn't start sing-box: the run directory could not be written"
    );
    // macOS: another account owns the helper, and Settings › TUN updates it.
    let mac = |code, message| error_message_on(code, message, HelperOs::MacOs);
    assert_eq!(mac(ErrorCode::Unauthorized, "x"), EN.helper.mac_not_allowed);
    assert_eq!(
        mac(ErrorCode::VersionMismatch, "x"),
        EN.helper.mac_version_mismatch
    );
    assert_eq!(mac(ErrorCode::Busy, "x"), EN.helper.busy);
    assert_eq!(
        error_message(ErrorCode::Busy, "x"),
        error_message_on(ErrorCode::Busy, "x", HelperOs::CURRENT)
    );
}

#[test]
fn every_exit_code_reads() {
    let known = [
        exit::USAGE,
        exit::UNSUPPORTED_OS,
        exit::HELPER_DIR_REFUSED,
        exit::STATE_DIR_REFUSED,
        exit::MANIFEST_REFUSED,
        exit::PIPE_SQUATTED,
        exit::PIPE_FAILED,
        exit::CONSOLE_ELEVATED,
        exit::PRIVILEGES_REFUSED,
        exit::SOCKET_FAILED,
        exit::NOT_ROOT,
        exit::INTERNAL,
    ];
    for os in [HelperOs::Windows, HelperOs::MacOs] {
        let messages: Vec<String> = known
            .iter()
            .map(|code| exit_code_message_on(*code, os))
            .collect();
        for (code, message) in known.iter().zip(&messages) {
            assert!(
                message.starts_with("The privileged helper stopped: "),
                "{message}"
            );
            assert!(
                !message.contains("exit code"),
                "{code} has its own words: {message}"
            );
        }
        let distinct: std::collections::BTreeSet<_> = messages.iter().collect();
        assert_eq!(distinct.len(), known.len());
        assert_eq!(
            exit_code_message_on(99, os),
            "The privileged helper stopped: exit code 99."
        );
    }
    assert_eq!(
        exit_code_message_on(exit::MANIFEST_REFUSED, HelperOs::Windows),
        "The privileged helper stopped: its copy of sing-box doesn't match what was installed; reinstall BoxPilot."
    );
    // On macOS, reinstalling BoxPilot doesn't repair the helper; Settings
    // › TUN does.
    assert_eq!(
        exit_code_message_on(exit::MANIFEST_REFUSED, HelperOs::MacOs),
        "The privileged helper stopped: its copy of sing-box doesn't match what was installed; reinstall it in Settings › TUN."
    );
    for code in [
        exit::HELPER_DIR_REFUSED,
        exit::STATE_DIR_REFUSED,
        exit::MANIFEST_REFUSED,
    ] {
        let mac = exit_code_message_on(code, HelperOs::MacOs);
        assert!(mac.contains("Settings › TUN"), "{mac}");
        assert!(!mac.contains("BoxPilot"), "{mac}");
    }
    assert_eq!(
        exit_code_message(exit::INTERNAL),
        exit_code_message_on(exit::INTERNAL, HelperOs::CURRENT)
    );
}

#[test]
fn every_open_error_reads() {
    let windows = |error: OpenError| error.message_on(HelperOs::Windows);
    assert_eq!(windows(OpenError::NotInstalled), EN.helper.not_installed);
    assert_eq!(windows(OpenError::Disabled), EN.helper.disabled);
    assert_eq!(windows(OpenError::StartDenied), EN.helper.start_denied);
    assert_eq!(windows(OpenError::ConnectDenied), EN.helper.connect_denied);
    assert_eq!(windows(OpenError::TimedOut), EN.helper.timed_out);
    assert_eq!(windows(OpenError::Unsupported), EN.helper.unsupported);
    assert_eq!(
        windows(OpenError::ServiceExited(exit::PIPE_SQUATTED)),
        exit_code_message_on(exit::PIPE_SQUATTED, HelperOs::Windows)
    );
    assert_eq!(
        windows(OpenError::ServiceFailed(1053)),
        "The privileged helper failed to start (Windows error 1053). Reinstall BoxPilot to repair it."
    );
    assert_eq!(
        windows(OpenError::Os("boom".into())),
        "Couldn't reach the privileged helper: boom"
    );
    // macOS: Settings › TUN installs it, and Login Items may have turned it
    // off; nothing about the MSI or Services.
    let mac = |error: OpenError| error.message_on(HelperOs::MacOs);
    assert_eq!(mac(OpenError::NotInstalled), EN.helper.mac_not_installed);
    assert_eq!(mac(OpenError::Disabled), EN.helper.mac_turned_off);
    assert_eq!(mac(OpenError::ConnectDenied), EN.helper.mac_connect_denied);
    assert_eq!(
        mac(OpenError::ServiceExited(exit::STATE_DIR_REFUSED)),
        exit_code_message_on(exit::STATE_DIR_REFUSED, HelperOs::MacOs)
    );
    for error in [
        OpenError::NotInstalled,
        OpenError::Disabled,
        OpenError::ConnectDenied,
    ] {
        let text = mac(error.clone());
        assert!(text.contains("Settings › TUN"), "{text}");
        assert!(
            !text.contains("MSI") && !text.contains("Services"),
            "{text}"
        );
        assert_eq!(error.message(), error.message_on(HelperOs::CURRENT));
    }
    assert!(mac(OpenError::Disabled).contains("Login Items"));
}

/// On Linux there is no helper to reach, and BoxPilot is never "elevated"
/// in the helper's sense.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
#[test]
fn no_helper_on_linux() {
    assert!(matches!(open(), Err(OpenError::Unsupported)));
    assert!(!process_is_elevated());
}
