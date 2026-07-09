//! Integration harness driven by `tests/import_rollback.rs` as a subprocess
//! (headless gpui can't run inside the test binary: on macOS `quit()` goes
//! through `NSApp terminate:` and never returns from `run`).
//!
//! Boots a real headless gpui App + `AppState` against a temp data dir
//! (`BOXPILOT_DATA_DIR`) and exercises the `sing-box://` URI-import failure
//! path end to end. Home keys its "Add subscription" empty card on
//! `settings.has_profiles()`, so a failed import must not leave a profile
//! behind — otherwise the card disappears and the failure reads as success.
//!
//! Exit codes: 0 = expected behavior, 1 = regression, 2 = inconclusive.

use box_pilot_gui::core::deeplink::ImportRequest;
use box_pilot_gui::core::settings::ProfileSource;
use box_pilot_gui::state::AppState;
use gpui::{AsyncApp, Entity};
use std::time::Duration;

/// Discard port: connection refused within milliseconds — the same
/// `update_profile` `Err` arm a sing-box-rejected config lands in.
const URL_A: &str = "http://127.0.0.1:9/a.json";
const URL_B: &str = "http://127.0.0.1:9/b.json";
const URL_C: &str = "http://127.0.0.1:9/c.json";

fn req(url: &str) -> ImportRequest {
    ImportRequest {
        url: url.to_string(),
        name: Some("Harness".to_string()),
    }
}

/// Poll until no fetch is in flight (HTTP timeout is 8s; connection-refused
/// fails in milliseconds). Exits 2 if the update never settles.
async fn wait_idle(app_state: &Entity<AppState>, cx: &mut AsyncApp) {
    for _ in 0..150 {
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
        if !cx.update(|cx| app_state.read(cx).is_updating()) {
            return;
        }
    }
    eprintln!("[harness] INCONCLUSIVE: update still in flight after 15s");
    std::process::exit(2);
}

fn check(app_state: &Entity<AppState>, cx: &mut AsyncApp, want_profiles: usize, label: &str) {
    let (n, active) = cx.update(|cx| {
        let s = app_state.read(cx);
        (
            s.settings.profiles.len(),
            s.settings.active_profile_id.clone(),
        )
    });
    if n != want_profiles {
        eprintln!(
            "[harness] FAIL {}: profiles={} (want {}) active={:?}",
            label, n, want_profiles, active
        );
        std::process::exit(1);
    }
    eprintln!("[harness] ok {}: profiles={} active={:?}", label, n, active);
}

fn main() {
    let tmp = std::env::temp_dir().join(format!("boxpilot-harness-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).expect("create temp data dir");
    std::env::set_var("BOXPILOT_DATA_DIR", &tmp);

    let (_tx, rx) = futures_channel::mpsc::unbounded::<String>();

    gpui_platform::headless().run(move |cx| {
        let app_state = AppState::new(rx, cx);
        assert!(
            !app_state.read(cx).settings.has_profiles(),
            "harness must start in the empty state"
        );

        cx.spawn(async move |cx| {
            // A — the reported bug: a failed import from the empty state must
            // return to the empty state (keep the "Add subscription" card).
            cx.update(|cx| {
                app_state.update(cx, |s, cx| s.import_profile(req(URL_A), cx));
            });
            wait_idle(&app_state, cx).await;
            check(&app_state, cx, 0, "A failed-import rolls back");

            // B — impatient double-click of the same link: the second import
            // cancels the first mid-flight (its failure arm never runs), so
            // the first import's created profile must be rolled back on
            // cancellation, not only on failure.
            cx.update(|cx| {
                app_state.update(cx, |s, cx| {
                    s.import_profile(req(URL_B), cx);
                    s.import_profile(req(URL_B), cx);
                });
            });
            wait_idle(&app_state, cx).await;
            check(&app_state, cx, 0, "B cancelled+failed imports roll back");

            // C — guard against over-eager rollback: re-importing the URL of
            // a profile the user created explicitly (dialog path) must KEEP
            // that profile when the fetch fails.
            cx.update(|cx| {
                app_state.update(cx, |s, cx| {
                    s.create_profile(
                        "Dialog".to_string(),
                        ProfileSource::Remote {
                            url: URL_C.to_string(),
                            auto_update_interval_minutes: 0,
                        },
                        cx,
                    );
                    s.import_profile(req(URL_C), cx);
                });
            });
            wait_idle(&app_state, cx).await;
            check(&app_state, cx, 1, "C reuse-import failure keeps the profile");

            eprintln!("[harness] all scenarios passed");
            std::process::exit(0);
        })
        .detach();
    });
}
