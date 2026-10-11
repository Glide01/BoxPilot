//! What the macOS helper does around sing-box (ADR 0006 rule 6, "System
//! proxy"), as `cleanup` decided it: once sing-box is up, the DNS flush and
//! the system proxy that its sandbox denies sing-box itself
//! ([`set_up`]); after it, the system proxy reset, with ADR 0005's
//! conservative rule (`boxpilot_runconfig::system_proxy`), and the DNS
//! caches, which only root can flush whole ([`carry_out`]).
//!
//! **No shell, ever.** Each tool runs by its absolute path, from a SIP-
//! protected system directory, with an argv (a network service's name is
//! one argument, whatever it holds), an environment of only a system
//! `PATH`, stdin from `/dev/null`, and a deadline: a tool that hangs is
//! killed, and the helper goes on.

use crate::cleanup::{AfterStart, Cleanup};
use crate::helper_log;
use crate::spawnplan::POSIX_PATH;
use boxpilot_runconfig::system_proxy::{
    default_route_interface, macos_proxy_is_ours, parse_network_services, service_for_device,
    LIST_SERVICES, LIST_SERVICE_ORDER, MACOS_PROXY_HOST, MACOS_PROXY_KINDS, MACOS_PROXY_SETTERS,
    NETWORKSETUP,
};
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Flushes the directory services cache.
pub const DSCACHEUTIL: &str = "/usr/bin/dscacheutil";
/// Sends mDNSResponder SIGHUP, which flushes its cache.
pub const KILLALL: &str = "/usr/bin/killall";
/// Names the default route's interface (`route -n get default`).
pub const ROUTE: &str = "/sbin/route";

/// The longest any tool may take.
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);
/// The most of a tool's output the helper reads.
const MAX_OUTPUT: u64 = 1024 * 1024;

/// Once sing-box is up: carry out `plan`.
pub fn set_up(plan: &AfterStart) {
    if plan.flush_dns {
        flush_dns();
    }
    if let Some(port) = plan.set_proxy {
        set_proxy(port);
    }
}

/// Set the SOCKS, web and secure web proxies of the default route's
/// network service to `127.0.0.1:<port>`, as sing-box's `set_system_proxy`
/// would (`common/settings/proxy_darwin.go`): the same service, the same
/// three proxies, no bypass list. Best effort: what can't be found or set
/// is logged, and the run goes on; the reset after it is the conservative
/// one either way. (Unlike sing-box, the helper doesn't follow the default
/// interface when it changes during a run: TUN carries that traffic all
/// the same.)
fn set_proxy(port: u16) {
    let Some(route) = run(ROUTE, &["-n", "get", "default"]) else {
        helper_log!("no default route: the system proxy isn't set");
        return;
    };
    let Some(device) = default_route_interface(&route) else {
        helper_log!("the default route names no interface: the system proxy isn't set");
        return;
    };
    let Some(order) = run(NETWORKSETUP, &[LIST_SERVICE_ORDER]) else {
        helper_log!("networksetup could not list the network services in order");
        return;
    };
    let Some(service) = service_for_device(&order, device) else {
        helper_log!("no enabled network service on {device}: the system proxy isn't set");
        return;
    };
    let port_text = port.to_string();
    let mut set = 0;
    for setter in MACOS_PROXY_SETTERS {
        match run(
            NETWORKSETUP,
            &[setter, service, MACOS_PROXY_HOST, &port_text],
        ) {
            Some(_) => set += 1,
            None => helper_log!("networksetup {setter} {service:?} failed"),
        }
    }
    helper_log!("set {set} proxies of {service:?} ({device}) to {MACOS_PROXY_HOST}:{port}");
}

/// After a run, or a crash: carry out `plan`.
pub fn carry_out(plan: &Cleanup) {
    if let Some(port) = plan.reset_proxy {
        reset_proxy(port);
    }
    if plan.flush_dns {
        flush_dns();
    }
}

/// Turn off, in every network service, the web, secure web and SOCKS
/// proxies still enabled on `127.0.0.1:<port>`. Best effort: a service
/// whose state can't be read is skipped.
fn reset_proxy(port: u16) {
    let Some(services) = run(NETWORKSETUP, &[LIST_SERVICES]) else {
        helper_log!("networksetup could not list the network services");
        return;
    };
    let mut reset = 0;
    for service in parse_network_services(&services) {
        for (get, set_state) in MACOS_PROXY_KINDS {
            let Some(state) = run(NETWORKSETUP, &[get, service]) else {
                continue;
            };
            if !macos_proxy_is_ours(&state, Some(port)) {
                continue;
            }
            match run(NETWORKSETUP, &[set_state, service, "off"]) {
                Some(_) => reset += 1,
                None => helper_log!("networksetup {set_state} {service:?} off failed"),
            }
        }
    }
    helper_log!("turned off {reset} proxies still on 127.0.0.1:{port}");
}

/// Flush the DNS caches: the directory services' and mDNSResponder's.
fn flush_dns() {
    if run(DSCACHEUTIL, &["-flushcache"]).is_none() {
        helper_log!("dscacheutil -flushcache failed");
    }
    if run(KILLALL, &["-HUP", "mDNSResponder"]).is_none() {
        helper_log!("killall -HUP mDNSResponder failed");
    }
}

/// Run `program`, by its absolute path, with `args` and nothing else; its
/// standard output if it exits 0 by the deadline.
fn run(program: &str, args: &[&str]) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", POSIX_PATH)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stdout.take(MAX_OUTPUT).read_to_end(&mut output);
        output
    });
    let deadline = Instant::now() + TOOL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                helper_log!("{program} took too long: killed");
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let output = reader.join().ok()?;
    status
        .filter(|status| status.success())
        .map(|_| String::from_utf8_lossy(&output).into_owned())
}
