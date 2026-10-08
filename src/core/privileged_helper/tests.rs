//! The pure half of the helper client. Messages are checked in English
//! (`s()` without `set_language`, as every core test does).

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
    assert_eq!(HELPER_PLATFORM, cfg!(target_os = "windows"));
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
    assert_eq!(
        error_message(ErrorCode::Unauthorized, "x"),
        EN.helper.not_allowed
    );
    assert_eq!(
        error_message(ErrorCode::VersionMismatch, "x"),
        EN.helper.version_mismatch
    );
    assert_eq!(error_message(ErrorCode::Busy, "x"), EN.helper.busy);
    assert_eq!(
        error_message(ErrorCode::BadRequest, "`config_len` is 0"),
        "The privileged helper didn't accept BoxPilot's request: `config_len` is 0"
    );
    assert_eq!(
        error_message(
            ErrorCode::Internal,
            "the run directory could not be written"
        ),
        "The privileged helper couldn't start sing-box: the run directory could not be written"
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
        exit::INTERNAL,
    ];
    let messages: Vec<String> = known.iter().map(|code| exit_code_message(*code)).collect();
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
        exit_code_message(exit::MANIFEST_REFUSED),
        "The privileged helper stopped: its copy of sing-box doesn't match what was installed; reinstall BoxPilot."
    );
    assert_eq!(
        exit_code_message(99),
        "The privileged helper stopped: exit code 99."
    );
}

#[test]
fn every_open_error_reads() {
    assert_eq!(OpenError::NotInstalled.message(), EN.helper.not_installed);
    assert_eq!(OpenError::Disabled.message(), EN.helper.disabled);
    assert_eq!(OpenError::StartDenied.message(), EN.helper.start_denied);
    assert_eq!(OpenError::ConnectDenied.message(), EN.helper.connect_denied);
    assert_eq!(OpenError::TimedOut.message(), EN.helper.timed_out);
    assert_eq!(OpenError::Unsupported.message(), EN.helper.unsupported);
    assert_eq!(
        OpenError::ServiceExited(exit::PIPE_SQUATTED).message(),
        exit_code_message(exit::PIPE_SQUATTED)
    );
    assert_eq!(
        OpenError::ServiceFailed(1053).message(),
        "The privileged helper failed to start (Windows error 1053). Reinstall BoxPilot to repair it."
    );
    assert_eq!(
        OpenError::Os("boom".into()).message(),
        "Couldn't reach the privileged helper: boom"
    );
}

/// Off Windows there is no helper to reach, and BoxPilot is never
/// "elevated" in the helper's sense.
#[cfg(not(target_os = "windows"))]
#[test]
fn no_helper_off_windows() {
    assert!(matches!(open(), Err(OpenError::Unsupported)));
    assert!(!process_is_elevated());
}
