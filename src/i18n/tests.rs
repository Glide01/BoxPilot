//! Table checks. Never `set_language` here: tests share the process-wide
//! language and run in parallel — read `EN` / `ZH_CN` directly.

use super::*;

#[test]
fn locale_tags_pick_the_language() {
    for zh in [
        "zh",
        "zh-CN",
        "zh_CN.UTF-8",
        "zh-Hans-SG",
        "zh-TW",
        "zh_HK",
        "ZH-cn",
        " zh-CN ",
    ] {
        assert_eq!(
            language_for_locale(Some(zh)),
            Language::SimplifiedChinese,
            "{zh:?}"
        );
    }
    for other in ["en-US", "en", "ja-JP", "de_DE.UTF-8", "zha", "", "C", "POSIX"] {
        assert_eq!(language_for_locale(Some(other)), Language::English, "{other:?}");
    }
    assert_eq!(language_for_locale(None), Language::English);
}

#[test]
fn explicit_preferences_ignore_the_os() {
    assert_eq!(resolve(LanguagePreference::English), Language::English);
    assert_eq!(
        resolve(LanguagePreference::SimplifiedChinese),
        Language::SimplifiedChinese
    );
}

#[test]
fn languages_map_to_their_tables_and_component_locales() {
    assert!(std::ptr::eq(Language::English.strings(), &EN));
    assert!(std::ptr::eq(Language::SimplifiedChinese.strings(), &ZH_CN));
    assert_eq!(Language::English.component_locale(), "en");
    assert_eq!(Language::SimplifiedChinese.component_locale(), "zh-CN");
    for language in [Language::English, Language::SimplifiedChinese] {
        assert_eq!(Language::from_u8(language.to_u8()), language);
    }
}

/// A hand-picked spread of plain strings from every area.
fn plain(t: &Strings) -> Vec<&'static str> {
    vec![
        t.common.cancel,
        t.common.copied,
        t.status.disconnected,
        t.status.starting,
        t.status.connected,
        t.status.connect,
        t.status.disconnect,
        t.time.just_now,
        t.nav.home,
        t.nav.groups,
        t.nav.connections,
        t.nav.profiles,
        t.nav.logs,
        t.nav.tools,
        t.nav.settings,
        t.home.no_subscription_title,
        t.home.clash_mode,
        t.home.proxy_mode,
        t.home.mode_proxy,
        t.home.system_proxy,
        t.profiles.add_title,
        t.profiles.interval,
        t.profiles.delete_body,
        t.usage.expires_today,
        t.groups.search_placeholder,
        t.groups.test_all,
        t.groups.timeout,
        t.connections.filter_placeholder,
        t.connections.close_all,
        t.logs.configured_level,
        t.tools.quality_hint,
        t.tools.nat_full_cone_hint,
        t.tailscale.waiting_approval,
        t.tailscale.https_hint,
        t.vpn.step_too_new,
        t.vpn.default_server_hint,
        t.settings.language,
        t.settings.follow_system,
        t.settings.appearance,
        t.settings.close_minimize,
        t.settings.allow_lan,
        t.settings.clear_cache_hint,
        t.updates.check_automatically,
        t.updates.rate_limited,
        t.config_viewer.preview_hint,
        t.chart.last_two_minutes,
        t.tray.show,
        t.tray.quit,
        t.close_dialog.body,
        t.dialogs.import_title,
        t.dialogs.tun_grant_body,
        t.messages.ready,
        t.messages.api_port_retry,
        t.errors.invalid_sub_url,
        t.errors.tun_dismissed,
    ]
}

#[test]
fn no_string_is_empty() {
    for t in [&EN, &ZH_CN] {
        for text in plain(t) {
            assert!(!text.trim().is_empty());
        }
    }
}

#[test]
fn chinese_is_translated() {
    // Same text in both tables means a string was copied, not translated
    // (product names and protocol words are allowed to match).
    let same: Vec<_> = plain(&EN)
        .into_iter()
        .zip(plain(&ZH_CN))
        .filter(|(en, zh)| en == zh)
        .collect();
    assert!(same.is_empty(), "untranslated: {same:?}");
    assert_eq!(ZH_CN.status.connect, "连接");
    assert_eq!(ZH_CN.status.disconnect, "断开");
    assert_eq!(ZH_CN.status.connected, "已连接");
    assert_eq!(ZH_CN.home.clash_mode, "Clash 模式");
    assert_eq!(ZH_CN.groups.test, "测速");
}

/// sing-box is never "内核" (CONTEXT.md).
#[test]
fn chinese_never_calls_sing_box_the_core() {
    let text = format!(
        "{} {} {} {}",
        plain(&ZH_CN).join(" "),
        (ZH_CN.messages.sing_box_too_old)("1.13.0", "1.14.0"),
        (ZH_CN.messages.start_failed)("sing-box", "a.json", "x"),
        (ZH_CN.vpn.unknown_step)("x"),
    );
    assert!(!text.contains("内核"));
}

#[test]
fn formatted_messages_fill_in_their_values() {
    assert_eq!((EN.home.running_for)("1h 23m"), "Running for 1h 23m");
    assert_eq!((ZH_CN.home.running_for)("1小时23分"), "已运行 1小时23分");

    assert_eq!((EN.time.days_ago)(1), "1 day ago");
    assert_eq!((EN.time.days_ago)(3), "3 days ago");
    assert_eq!((ZH_CN.time.days_ago)(3), "3 天前");
    assert_eq!((ZH_CN.time.minutes_ago)(5), "5 分钟前");

    assert_eq!((EN.usage.expires_in_days)(1), "expires in 1 day");
    assert_eq!((EN.usage.expires_in_days)(12), "expires in 12 days");
    assert_eq!((ZH_CN.usage.expires_in_days)(12), "12 天后到期");
    assert_eq!(
        (EN.usage.alert)("Work", "92% of traffic used"),
        "\"Work\" subscription: 92% of traffic used."
    );
    assert_eq!(
        (ZH_CN.usage.alert)("Work", "流量已用 92%"),
        "订阅「Work」：流量已用 92%。"
    );

    assert_eq!(
        (EN.profiles.auto_update_every)("https://a/s", 30),
        "https://a/s · auto-update 30m"
    );
    assert_eq!(
        (ZH_CN.profiles.auto_update_every)("https://a/s", 30),
        "https://a/s · 每 30 分钟自动更新"
    );

    assert_eq!((EN.connections.active_tab)(3), "Active (3)");
    assert_eq!((ZH_CN.connections.active_tab)(3), "活动 (3)");
    assert_eq!((EN.logs.count_of)(3, 10), "3 of 10");
    assert_eq!((ZH_CN.logs.count_of)(3, 10), "3 / 10");

    assert_eq!((EN.vpn.cookies)("a", 1), "the a cookie");
    assert_eq!((EN.vpn.cookies)("a, b", 2), "the a, b cookies");
    assert_eq!((ZH_CN.vpn.cookies)("a, b", 2), "Cookie a, b");

    assert_eq!((EN.tray.tooltip)(EN.status.connected), "BoxPilot — Connected");
    assert_eq!((ZH_CN.tray.tooltip)(ZH_CN.status.connected), "BoxPilot — 已连接");

    assert_eq!(
        (EN.updates.available_toast)("1.14.0"),
        "BoxPilot 1.14.0 is available — see Settings › About."
    );
    assert_eq!((ZH_CN.chart.mins_secs_ago)(1, 5), "1 分 5 秒前");
    assert_eq!((ZH_CN.settings.copied_command)("fish"), "已复制 fish 代理命令。");
}

#[test]
fn chinese_uses_full_width_punctuation() {
    // ASCII ',' ':' ';' right after a CJK character reads wrong in Chinese.
    let samples = [
        ZH_CN.profiles.interval.to_string(),
        ZH_CN.profiles.delete_body.to_string(),
        ZH_CN.settings.lan_off.to_string(),
        (ZH_CN.messages.clash_mode_failed)("x"),
        (ZH_CN.errors.read_failed)("a", "b"),
        (ZH_CN.settings.lan_on_at)("192.168.1.2:7788"),
    ];
    for sample in samples {
        let chars: Vec<char> = sample.chars().collect();
        for pair in chars.windows(2) {
            let cjk = ('\u{4e00}'..='\u{9fff}').contains(&pair[0]);
            assert!(
                !(cjk && matches!(pair[1], ',' | ':' | ';' | '(' | ')')),
                "{sample:?}"
            );
        }
    }
}
