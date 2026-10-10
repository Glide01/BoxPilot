//! Integration harness driven by `tests/import_rollback.rs` as a subprocess
//! (headless gpui can't run inside the test binary: on macOS `quit()` goes
//! through `NSApp terminate:` and never returns from `run`).
//!
//! Boots a real headless gpui App + `AppState` against a temp data dir
//! (`BOXPILOT_DATA_DIR`) and exercises the `sing-box://` URI-import failure
//! path end to end. Home keys its "Add subscription" empty card on
//! `settings.has_profiles()`, so a failed import must not leave a profile
//! behind — otherwise the card disappears and the failure reads as success.
//! Then, against a local HTTP server that holds its answer: a fetch asked
//! for while another is in flight is queued, not dropped, and a fetch whose
//! profile is deleted mid-flight writes nothing.
//!
//! Exit codes: 0 = expected behavior, 1 = regression, 2 = inconclusive.

use box_pilot_gui::core::deeplink::{ImportRequest, LaunchAttempt};
use box_pilot_gui::core::paths::profile_config_path;
use box_pilot_gui::core::settings::ProfileSource;
use box_pilot_gui::state::app_state::FetchOrigin;
use box_pilot_gui::state::AppState;
use gpui::{AsyncApp, Entity};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::mpsc;
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

/// The smallest config `strip_inbounds` accepts. No sing-box binary sits
/// next to the harness, so `sing-box check` is skipped.
const CONFIG_BODY: &str = r#"{"outbounds":[{"type":"direct","tag":"direct"}]}"#;

/// Local HTTP server answering one request at a time with `CONFIG_BODY`,
/// but only once `release` is sent: a fetch is reliably in flight meanwhile.
struct HeldServer {
    url: String,
    accepted: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
    answered: mpsc::Receiver<()>,
}

fn held_server() -> HeldServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind harness server");
    let url = format!("http://{}/sub.json", listener.local_addr().unwrap());
    let (accepted_tx, accepted) = mpsc::channel();
    let (release, release_rx) = mpsc::channel::<()>();
    let (answered_tx, answered) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let _ = accepted_tx.send(());
            if release_rx.recv().is_err() {
                return;
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                CONFIG_BODY.len(),
                CONFIG_BODY
            );
            let _ = stream.write_all(response.as_bytes());
            drop(stream);
            let _ = answered_tx.send(());
        }
    });
    HeldServer {
        url,
        accepted,
        release,
        answered,
    }
}

/// Poll `rx` on the executor's timer (blocking would stall the very fetch
/// being waited for). Exits 2 after 15s.
async fn wait_for(rx: &mpsc::Receiver<()>, what: &str, cx: &mut AsyncApp) {
    for _ in 0..300 {
        if rx.try_recv().is_ok() {
            return;
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
    eprintln!("[harness] INCONCLUSIVE: {} never happened", what);
    std::process::exit(2);
}

fn remote(url: &str) -> ProfileSource {
    ProfileSource::Remote {
        url: url.to_string(),
        auto_update_interval_minutes: 0,
        update_via_sing_box: true,
    }
}

fn fail(label: &str, detail: String) -> ! {
    eprintln!("[harness] FAIL {}: {}", label, detail);
    std::process::exit(1);
}

/// Temp files left in `configs/` (staged configs that were never cleaned up).
fn temp_files(configs: &Path) -> Vec<String> {
    std::fs::read_dir(configs)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".tmp"))
                .collect()
        })
        .unwrap_or_default()
}

fn main() {
    let tmp = std::env::temp_dir().join(format!("boxpilot-harness-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).expect("create temp data dir");
    std::env::set_var("BOXPILOT_DATA_DIR", &tmp);

    // Nothing is ever sent: this harness drives `import_profile` directly,
    // so the launch-attempt gate (never opened here — there is no view) is
    // out of scope.
    let (_tx, rx) = futures_channel::mpsc::unbounded::<LaunchAttempt>();

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
                            update_via_sing_box: true,
                        },
                        cx,
                    );
                    s.import_profile(req(URL_C), cx);
                });
            });
            wait_idle(&app_state, cx).await;
            check(&app_state, cx, 1, "C reuse-import failure keeps the profile");
            // …and says why on its update button, without counting as
            // checked.
            let kept_error = cx.update(|cx| {
                let s = app_state.read(cx);
                s.settings.profiles.first().is_some_and(|p| {
                    s.fetch_error(&p.id).is_some() && p.last_checked_secs.is_none()
                })
            });
            if !kept_error {
                fail("C failure reason", "no fetch error kept".to_string());
            }

            let app_dir = cx.update(|cx| app_state.read(cx).app_dir.clone());
            let server = held_server();

            // D — the Add dialog's first fetch while another fetch is in
            // flight is queued and runs after it, instead of being dropped.
            let source_file = app_dir.join("local-source.json");
            std::fs::write(&source_file, CONFIG_BODY).expect("write local source");
            let held_id = cx.update(|cx| {
                app_state.update(cx, |s, cx| {
                    let id = s.create_profile("Held".to_string(), remote(&server.url), cx);
                    s.update_profile(id.clone(), FetchOrigin::Manual, cx);
                    id
                })
            });
            wait_for(&server.accepted, "D: held fetch reached the server", cx).await;
            let queued_id = cx.update(|cx| {
                app_state.update(cx, |s, cx| {
                    let id = s.create_profile(
                        "Queued".to_string(),
                        ProfileSource::Local {
                            path: source_file.display().to_string(),
                        },
                        cx,
                    );
                    s.update_profile(id.clone(), FetchOrigin::Manual, cx);
                    id
                })
            });
            let _ = server.release.send(());
            wait_idle(&app_state, cx).await;
            for id in [&held_id, &queued_id] {
                if !profile_config_path(&app_dir, id).exists() {
                    fail("D queued fetch runs", format!("no config for {}", id));
                }
                let (checked, error) = cx.update(|cx| {
                    let s = app_state.read(cx);
                    let checked = s
                        .settings
                        .profiles
                        .iter()
                        .find(|p| &p.id == id)
                        .and_then(|p| p.last_checked_secs);
                    (checked, s.fetch_error(id).map(str::to_string))
                });
                if checked.is_none() || error.is_some() {
                    fail(
                        "D success stamps checked",
                        format!("{}: checked {:?}, error {:?}", id, checked, error),
                    );
                }
            }
            eprintln!("[harness] ok D queued fetch runs after the held one");

            // E — deleting a profile mid-fetch: the fetch still completes in
            // the background, but must neither write its config nor leave
            // its staged temp file; and the id is not handed out again.
            let doomed_id = cx.update(|cx| {
                app_state.update(cx, |s, cx| {
                    let id = s.create_profile("Doomed".to_string(), remote(&server.url), cx);
                    s.update_profile(id.clone(), FetchOrigin::Manual, cx);
                    id
                })
            });
            wait_for(&server.accepted, "E: doomed fetch reached the server", cx).await;
            cx.update(|cx| {
                app_state.update(cx, |s, cx| s.delete_profile(doomed_id.clone(), cx));
            });
            let _ = server.release.send(());
            wait_for(&server.answered, "E: server answered", cx).await;
            // The background job reads, strips and stages after the answer.
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            let doomed_config = profile_config_path(&app_dir, &doomed_id);
            if doomed_config.exists() {
                fail(
                    "E deleted profile's fetch",
                    format!("{} was written", doomed_config.display()),
                );
            }
            let leftovers = temp_files(&app_dir.join("configs"));
            if !leftovers.is_empty() {
                fail(
                    "E deleted profile's fetch",
                    format!("temp files left: {:?}", leftovers),
                );
            }
            let next_id = cx.update(|cx| {
                app_state.update(cx, |s, cx| {
                    s.create_profile("Next".to_string(), remote(&server.url), cx)
                })
            });
            if next_id == doomed_id {
                fail("E id reuse", format!("{} handed out again", next_id));
            }
            eprintln!("[harness] ok E deleted profile's fetch writes nothing");

            eprintln!("[harness] all scenarios passed");
            std::process::exit(0);
        })
        .detach();
    });
}
