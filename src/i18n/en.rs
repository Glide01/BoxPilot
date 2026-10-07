//! English.

use super::*;

pub static EN: Strings = Strings {
    common: Common {
        ok: "OK",
        cancel: "Cancel",
        close: "Close",
        save: "Save",
        save_as: "Save…",
        delete: "Delete",
        copy: "Copy",
        copied: "Copied",
        search: "Search",
        start: "Start",
        stop: "Stop",
        unknown: "Unknown",
        loading: "Loading…",
        view: "View",
        download: "Download",
        upload: "Upload",
        colon: ": ",
    },
    status: Status {
        disconnected: "Disconnected",
        starting: "Starting…",
        connected: "Connected",
        connect: "Connect",
        disconnect: "Disconnect",
    },
    time: Time {
        just_now: "just now",
        minutes_ago: |n| format!("{n} min ago"),
        hours_ago: |n| format!("{n} hr ago"),
        days_ago: |n| {
            if n == 1 {
                "1 day ago".to_string()
            } else {
                format!("{n} days ago")
            }
        },
        day: "d",
        hour: "h",
        minute: "m",
        second: "s",
        unit_sep: " ",
        coarse_secs: |n| format!("{n}s"),
        coarse_mins: |n| format!("{n} min"),
        coarse_hours_mins: |h, m| format!("{h} hr {m} min"),
        coarse_days_hours: |d, h| format!("{d} d {h} hr"),
    },
    nav: Nav {
        home: "Home",
        groups: "Groups",
        connections: "Connections",
        tailscale: "Tailscale",
        vpn: "VPN",
        profiles: "Profiles",
        logs: "Logs",
        tools: "Tools",
        settings: "Settings",
    },
    home: Home {
        no_subscription_title: "No subscription yet",
        no_subscription_hint: "Add one to get started",
        add_subscription: "Add subscription",
        clash_mode: "Clash Mode",
        memory: "Memory",
        connections: "Connections",
        uploaded: "Uploaded",
        downloaded: "Downloaded",
        proxy_mode: "Proxy Mode",
        mode_tun: "TUN",
        mode_proxy: "Proxy",
        system_proxy: "System Proxy",
        update: "Update",
        not_updated_yet: "not updated yet",
        running_for: |uptime| format!("Running for {uptime}"),
        ready_with: |profile| format!("Ready to connect with {profile}"),
        quick_settings: "Quick settings",
        subscription: "Subscription",
    },
    profiles: Profiles {
        title: "Profiles",
        add: "Add",
        add_title: "Add Profile",
        edit_title: "Edit Profile",
        name: "Name",
        name_placeholder: "Profile name…",
        kind_subscription: "Subscription",
        kind_local: "Local file",
        subscription_url: "Subscription URL",
        url_placeholder: "Enter subscription URL…",
        interval: "Auto-update interval (minutes, 0 = off)",
        config_file: "Config file",
        browse: "Browse…",
        no_file_selected: "No file selected",
        choose_json: "Please choose a .json config file.",
        delete_title: |name| format!("Delete profile \"{name}\"?"),
        delete_body: "Its downloaded config is removed too. This cannot be undone.",
        active: "Active",
        use_profile: "Use",
        never_updated: "never updated",
        empty: "No profiles yet",
        updated: |when| format!("updated {when}"),
        no_subscription_url: "No subscription URL",
        auto_update_every: |minutes| format!("Auto-updates every {minutes} min"),
        auto_update_off: "Auto-update off",
        local_file: "Local file",
        invalid_url: "Invalid URL",
        default_name: |n| format!("Profile {n}"),
        imported: "Imported",
    },
    usage: Usage {
        used: |amount| format!("{amount} used"),
        expires_today: "expires today",
        expires_in_days: |n| {
            if n == 1 {
                "expires in 1 day".to_string()
            } else {
                format!("expires in {n} days")
            }
        },
        expired_today: "expired today",
        expired_days_ago: |n| {
            if n == 1 {
                "expired 1 day ago".to_string()
            } else {
                format!("expired {n} days ago")
            }
        },
        used_up: "traffic used up",
        percent_used: |percent| format!("{percent}% of traffic used"),
        reason_sep: ", ",
        alert: |name, reason| format!("\"{name}\" subscription: {reason}."),
        expires_on: |date| format!("Expires {date} (UTC)"),
        as_of: |when| format!("Usage as of {when}"),
    },
    groups: Groups {
        title: "Groups",
        search_placeholder: "Search nodes or protocols…",
        sort: "Sort",
        sort_default: "Default",
        sort_delay: "Delay",
        test_all: "Test all",
        test_group: "Test group delay",
        test_delay: "Test delay",
        auto: "auto",
        timeout: "timeout",
        empty_title: "No node groups",
        empty_hint: "Connect to see node groups here.",
        no_match_title: "No matching nodes",
        no_match_hint: "Try a different search.",
    },
    connections: Connections {
        title: "Connections",
        filter_placeholder: "Filter by host, rule, chain, process…",
        open_count: |n| format!("{n} open"),
        total: "total",
        active_tab: "Active",
        closed_tab: "Closed",
        newest: "Newest",
        traffic: "Traffic",
        close_all: "Close all",
        close_connection: "Close connection",
        closed: "closed",
        empty_title: "No connections",
        empty_hint: "Connect to see live connections here.",
        no_match_title: "No matching connections",
        no_match_hint: "Try a different filter.",
        no_active_title: "No active connections",
        no_active_hint: "Connections appear here as apps use the proxy.",
        no_closed_title: "No closed connections",
        no_closed_hint: "Recently closed connections are kept here.",
    },
    connection_details: ConnectionDetails {
        close_panel: "Close details (Esc)",
        gone_title: "Connection no longer available",
        gone_hint: "sing-box no longer remembers this connection.",
        overview: "Overview",
        route: "Route",
        source_section: "Source",
        process_section: "Process",
        traffic_section: "Traffic",
        destination: "Destination",
        domain: "Domain",
        protocol: "Protocol",
        network: "Network",
        ip_version: "IP version",
        state: "State",
        active: "Active",
        closed: "Closed",
        inbound: "Inbound",
        rule: "Rule",
        chain: "Chain",
        outbound: "Outbound",
        from_outbound: "Detoured from",
        source_address: "Address",
        user: "User",
        process_name: "Name",
        process_path: "Path",
        process_id: "PID",
        process_user: "User",
        upload_speed: "Upload speed",
        download_speed: "Download speed",
        uploaded: "Uploaded",
        downloaded: "Downloaded",
        opened_at: "Opened",
        closed_at: "Closed",
        duration: "Duration",
    },
    logs: Logs {
        title: "Logs",
        count_of: |visible, total| format!("{visible} of {total}"),
        configured_level: "sing-box's configured log level",
        clear: "Clear",
        empty_title: "No logs yet",
        empty_hint: "Connect to start streaming sing-box output.",
    },
    tools: Tools {
        title: "Tools",
        not_running_title: "sing-box is not running",
        not_running_hint: "Connect to test network quality and NAT type.",
        outbound: "Outbound",
        search_outbounds: "Search outbounds",
        default_outbound: "Default outbound",
        quality_section: "Network quality",
        quality_hint: "Measures throughput and responsiveness (round trips per minute under load) \
                       through an outbound, the way Apple's networkQuality does.",
        mode: "Mode",
        mode_hint: "Parallel loads both directions at once; serial measures download, then upload.",
        parallel: "Parallel",
        serial: "Serial",
        max_runtime: "Max runtime",
        seconds: |n| format!("{n}s"),
        http3_hint: "Measure over QUIC instead of TCP.",
        config_url: "Config URL",
        config_url_hint: "A networkQuality config; empty uses Apple's test servers.",
        config_url_placeholder: "Default (Apple)",
        accuracy: |capacity, rpm| format!("Accuracy: {capacity}, RPM {rpm}"),
        idle_latency: "Idle latency",
        rpm_hint: "RPM: round trips per minute while the link is loaded — higher means \
                   more responsive. Accuracy: how stable each measurement became within the \
                   runtime.",
        accuracy_low: "Low",
        accuracy_medium: "Medium",
        accuracy_high: "High",
        done: "Done",
        failed: "Failed",
        cancelled: "Cancelled",
        fetching_config: "Fetching test config…",
        measuring_idle: "Measuring idle latency…",
        measuring_both: "Measuring download and upload…",
        measuring_download: "Measuring download…",
        measuring_upload: "Measuring upload…",
        finishing: "Finishing…",
        measuring: "Measuring…",
        progress_timeout: "Timed out waiting for progress from sing-box",
        ended_without_result: "sing-box ended the test without a result",
        stun_section: "NAT type (STUN)",
        stun_hint: "Sends STUN requests over UDP through an outbound to find the external \
                    address and how the NAT on that path maps and filters (RFC 5780). Through \
                    a proxy, this describes the proxy server's UDP path.",
        stun_server: "STUN server",
        stun_server_hint: "host[:port]; empty uses sing-box's default.",
        stun_binding: "Sending binding request…",
        stun_binding_answered: "Binding answered…",
        stun_mapping: "Detecting NAT mapping behavior…",
        stun_filtering: "Detecting NAT filtering behavior…",
        stun_testing: "Testing…",
        external_address: "External address",
        latency: "Latency",
        nat_mapping: "NAT mapping",
        nat_filtering: "NAT filtering",
        nat_unsupported:
            "This server doesn't support NAT type detection (RFC 5780 OTHER-ADDRESS). \
                          Try another STUN server to see mapping and filtering behaviour.",
        nat_endpoint_independent: "Endpoint Independent",
        nat_address_dependent: "Address Dependent",
        nat_address_port_dependent: "Address and Port Dependent",
        nat_full_cone: "Full cone (NAT1)",
        nat_restricted_cone: "Restricted cone (NAT2)",
        nat_port_restricted_cone: "Port-restricted cone (NAT3)",
        nat_symmetric: "Symmetric (NAT4)",
        nat_full_cone_hint: "Your external address is the same for every destination, and any \
                             host can reach it. The most open NAT: best for P2P, games and calls.",
        nat_restricted_cone_hint:
            "Your external address is the same for every destination, but only \
                                   hosts you have sent to can reach it (from any of their ports).",
        nat_port_restricted_cone_hint:
            "Your external address is the same for every destination, but only \
                                        the exact address and port you have sent to can reach it.",
        nat_independent_unknown_hint:
            "Your external address is the same for every destination. Filtering \
                                       behaviour could not be determined.",
        nat_symmetric_hint: "Every destination gets a different external port, so peers cannot \
                             reuse your address. P2P connections usually need a relay.",
        nat_dependent_hint: "Different destinations get different external mappings, so peers \
                             cannot reuse your address. P2P connections usually need a relay.",
    },
    tailscale: Tailscale {
        title: "Tailscale",
        empty_title: "No Tailscale endpoints",
        empty_hint: "Connect with a profile that has a Tailscale endpoint.",
        log_in: "Log in",
        log_in_tooltip: "Opens the Tailscale login page in your browser",
        log_out: "Log out",
        waiting_login_link: "Waiting for a login link from Tailscale…",
        log_in_hint: "Log in to add this device to your tailnet.",
        waiting_approval: "Waiting for a tailnet admin to approve this device.",
        tailnet: "Tailnet",
        this_device: "This device",
        dns_name: "DNS name",
        addresses: "Addresses",
        logout_title: "Log out of Tailscale?",
        logout_body: |tag| format!("\"{tag}\" leaves the tailnet until it logs in again."),
        logout_key_auth: " It logged in with an auth key, so getting back in needs a browser \
                          login or a new key.",
        exit_node: "Exit node",
        exit_node_on: "All traffic through this endpoint leaves via the exit node.",
        exit_node_off: "Traffic leaves from this device.",
        no_exit_nodes: "No device in the tailnet offers an exit node.",
        exit_node_none: "None",
        offline_choice: |name| format!("{name} (offline)"),
        ping_title: |name, ip| format!("Ping {name} ({ip})"),
        running: "running",
        waiting_reply: "Waiting for the first reply…",
        mark_read: "Mark as read",
        new_files: |n| format!("{n} new"),
        taildrop: "Taildrop",
        no_files_share: "No files received. Files other devices send here appear in this list.",
        no_files: "No files received.",
        receiving: |progress| format!("Receiving {progress}"),
        from_sender: |sender| format!("from {sender}"),
        save_dialog_failed: |e| format!("Couldn't open the save dialog: {e}"),
        delete_title: |name| format!("Delete \"{name}\"?"),
        delete_body: "The received file is removed from the Taildrop inbox. Saved copies are kept.",
        https_certs: "HTTPS certificates",
        https_hint: "Issued for this device's tailnet name. Needs HTTPS enabled for the tailnet.",
        get_certificate: "Get certificate",
        copy_certificate: "Copy certificate",
        certificate_copied: "Certificate copied.",
        certificate_title: |domain| format!("Certificate for {domain}"),
        certificate_body: |cert, key| {
            format!(
                "Save writes {cert} and {key} (the private key, not shown here) to a \
                 folder you choose, replacing older copies."
            )
        },
        save_here: "Save here",
        folder_dialog_failed: |e| format!("Couldn't open the folder dialog: {e}"),
        saved_pair: |cert, key| format!("Saved {cert} and {key}"),
        save_certificate_failed: |e| format!("Failed to save the certificate: {e}"),
        devices: "Devices",
        no_devices: "No other devices in the tailnet.",
        ping: "Ping",
        badge_exit_node: "Exit node",
        badge_exit_option: "exit node",
        badge_shared: "shared",
        badge_key_expired: "key expired",
        unknown_user: "Unknown user",
        online: "Online",
        last_seen: |when| format!("Last seen {when}"),
        offline: "Offline",
        ping_failed: |e| format!("Failed: {e}"),
        direct: "direct",
        direct_via: |endpoint| format!("direct ({endpoint})"),
        peer_relay: |relay| format!("peer relay ({relay})"),
        derp_region: |id| format!("DERP (region {id})"),
        relayed: "relayed",
        progress_of: |received, size, percent| format!("{received} of {size} ({percent}%)"),
        received: |amount| format!("{amount} received"),
        set_exit_node_failed: "Failed to set the exit node",
        logout_failed: "Failed to log out of Tailscale",
        logged_out: "Logged out of Tailscale.",
        mark_read_failed: "Failed to mark Taildrop files read",
        delete_failed: "Failed to delete the file",
        cancel_failed: "Failed to cancel the transfer",
        save_failed: "Failed to save the file",
        saved_to: |path| format!("Saved to {path}"),
        certificate_failed: |e| format!("Failed to get the certificate: {e}"),
        write_failed: |e| format!("Failed to write the file: {e}"),
        download_incomplete: |received, size| {
            format!("Download incomplete: received {received} of {size} bytes")
        },
        download_cancelled: "Download cancelled",
    },
    vpn: Vpn {
        title: "VPN",
        empty_title: "No VPN endpoints",
        empty_hint:
            "Connect with a profile that has OpenConnect, OpenVPN or USB/IP to see them here.",
        sign_in_title: |protocol, tag| format!("Sign in to {protocol} \"{tag}\""),
        sign_in: "Sign in",
        later: "Later",
        continue_: "Continue",
        cancel_sign_in: "Cancel sign-in",
        disconnect: "Disconnect",
        ended: "This sign-in request has ended.",
        open_sign_in_page: "Open sign-in page",
        callback_address: "Address your browser ended on",
        step_too_new: "This sign-in step is newer than BoxPilot understands. \
                       Update BoxPilot, or cancel it.",
        callback_intro: "Sign in on the server's page in your browser.",
        callback_body: |prefixes| {
            format!(
                "When you're done, the browser is sent to an address starting with {prefixes} — \
                 that page may fail to load, which is expected. Copy the full address \
                 from the address bar and paste it below."
            )
        },
        or: " or ",
        last_attempt_failed: |e| format!("The last attempt failed: {e}"),
        open_url_body: "The server wants you to sign in on a web page. Open it, \
                        finish signing in there, and the connection continues on \
                        its own.",
        unknown_step: |kind| {
            format!(
                "sing-box asks for a \"{kind}\" sign-in step, which this version of \
                 BoxPilot doesn't understand."
            )
        },
        username: "Username",
        password: "Password",
        account: |name| format!("Account: {name}"),
        response: "Response",
        waiting_for_sing_box: "Waiting for sing-box…",
        bus: |bus| format!("bus {bus}"),
        serial: |serial| format!("serial {serial}"),
        usbip_server: "USB/IP server",
        no_devices_shared: "No devices shared. A dynamic server shares the devices a client \
                            app lends it; BoxPilot doesn't lend this computer's devices.",
        no_status: "sing-box reports no status for it.",
        default_server_hint: "Shares this computer's matching USB devices. sing-box doesn't \
                              report their status for this kind of server.",
        connecting: "Connecting",
        waiting_sign_in: "Waiting for sign-in",
        connected: "Connected",
        error: "Error",
        failed: "failed",
        row_uptime: "Uptime",
        row_server: "Server",
        row_protocol: "Protocol",
        row_transport: "Transport",
        row_network: "Network",
        row_cipher: "Cipher",
        deadline_passed: "The server's time limit for this request has passed.",
        deadline_secs: |n| format!("The server waits {n} more seconds."),
        deadline_mins: |n| format!("The server waits about {n} more min."),
        cookies: |names, count| format!("the {names} cookie{}", if count == 1 { "" } else { "s" }),
        headers: |names, count| {
            format!(
                "the {names} response header{}",
                if count == 1 { "" } else { "s" }
            )
        },
        browser_limitation: |captured| {
            format!(
                "This server finishes single sign-on by handing {captured} to an embedded \
                 browser. BoxPilot signs in through your system browser, which can't \
                 pass them back, so this sign-in can't be completed here. Use a \
                 sing-box client with an embedded browser, set the endpoint's \
                 \"cookie\" option to an existing session, or ask your administrator \
                 for password sign-in (\"external_auth_disabled\")."
            )
        },
        stream_status: |protocol, e| format!("{protocol} status: {e}"),
        sign_in_failed: "Sign-in failed",
        cancel_sign_in_failed: "Couldn't cancel sign-in",
        form_changed: "The sign-in form changed; reopen it.",
        choose_value: |field| format!("Choose a value for {field}."),
        paste_address: "Paste the address your browser ended on.",
        address_mismatch: |prefixes| {
            format!("That address doesn't look like the sign-in result: it should start with {prefixes}.")
        },
        cannot_answer: "This request can't be answered here.",
        enter_username: "Enter a username.",
        enter_response: "Enter a response to the challenge.",
        usb_available: "Available",
        usb_in_use: "In use",
        usb_unavailable: "Unavailable",
        usb_wireless: "Wireless",
        usb_device: "USB device",
    },
    settings: Settings {
        title: "Settings",
        general: "General",
        network: "Network",
        tun: "TUN",
        shell_environment: "Shell environment",
        troubleshooting: "Troubleshooting",
        about: "About",
        language: "Language",
        language_hint: "System follows your desktop's language.",
        follow_system: "System",
        appearance: "Appearance",
        appearance_hint: "System follows your desktop's light or dark setting.",
        theme_light: "Light",
        theme_dark: "Dark",
        close_button: "Close button",
        close_hint: "While BoxPilot runs in the tray, sing-box stays connected.",
        close_no_tray_hint: "No system tray on this desktop to keep BoxPilot running in.",
        close_quits: "Quits BoxPilot",
        close_ask: "Ask",
        close_minimize: "Minimize to tray",
        close_quit: "Quit",
        local_proxy_port: "Local proxy port",
        allow_lan: "Allow LAN connections",
        lan_on_at: |at| {
            format!(
                "Other devices can use the proxy at {at} — no password; \
                 your firewall may ask to allow sing-box."
            )
        },
        lan_on_port: |port| {
            format!(
                "Other devices can use the proxy on port {port} once this computer \
                 is on a network — no password; your firewall may ask to allow sing-box."
            )
        },
        lan_off_at: |at| format!("Let other devices on your network use the proxy at {at}."),
        lan_off: "Let other devices on your network use the proxy.",
        ipv6: "IPv6",
        ipv6_hint: "Proxies IPv6 traffic in TUN mode.",
        clear_cache: "Clear Cache",
        clear_cache_hint:
            "Resets cache.db — node selections go back to defaults. Available while disconnected.",
        copied_command: |shell| format!("Copied {shell} proxy command."),
        running_config: "Running config",
        running_config_hint: "The exact config sing-box runs with.",
    },
    updates: Updates {
        updates: "Updates",
        check_automatically: "Check for updates automatically",
        check_automatically_hint: "Looks for a new BoxPilot release on GitHub once a day.",
        not_checked: "Not checked yet",
        checking: "Checking…",
        up_to_date: "Up to date",
        available: |version| format!("Version {version} available"),
        available_skipped: |version| format!("Version {version} available (skipped)"),
        failed: |reason| format!("Update check failed: {reason}"),
        download: "Download",
        skip: "Skip this version",
        check_now: "Check now",
        available_toast: |version| {
            format!("BoxPilot {version} is available — see Settings › About.")
        },
        unexpected_response: "unexpected response from GitHub",
        prerelease: "latest release is a prerelease",
        bad_tag: |tag| format!("unrecognised release tag \"{tag}\""),
        invalid_proxy: "invalid proxy address",
        client_setup: "couldn't set up the HTTP client",
        timed_out: "timed out",
        cannot_connect: "couldn't connect",
        interrupted: "connection interrupted",
        network_error: "network error",
        no_release: "no release published yet",
        rate_limited: "GitHub rate limit reached, try again later",
        http_status: |code| format!("GitHub returned HTTP {code}"),
    },
    config_viewer: ConfigViewer {
        title: "Running config",
        running: "Running",
        running_hint: "The config sing-box was started with (running_config.json).",
        preview: "Preview — not running",
        preview_hint: "What connecting now would run: the active profile with your current \
                       settings. BoxPilot picks its API port and secret at each start.",
        hide_credentials: "Hide credentials",
        hide_credentials_tooltip: "Masks passwords, keys, UUIDs and URL tokens. \
                                   BoxPilot's own API secret is always masked.",
        no_profile_title: "No profile yet",
        no_profile_hint:
            "Add a subscription or a local config on the Profiles page, and its config \
                          shows up here.",
        no_config_title: "This profile has no config yet",
        no_config_hint: "Update it on the Profiles page to download its config.",
        load_failed_title: "Couldn't load the config",
        search_tooltip: "Search (Ctrl+F)",
        open_folder: "Open folder",
        open_folder_tooltip: "Show the file in your file manager",
        not_json: |e| format!("Not valid JSON: {e}"),
        format_failed: |e| format!("Failed to format config: {e}"),
        profile_unreadable: |e| format!("The profile's config can't be read: {e}"),
    },
    chart: Chart {
        last_two_minutes: "Last 2 minutes",
        now: "Now",
        secs_ago: |s| format!("{s} s ago"),
        mins_ago: |m| format!("{m} min ago"),
        mins_secs_ago: |m, s| format!("{m} min {s} s ago"),
    },
    tray: Tray {
        show: "Show BoxPilot",
        system_proxy: "System Proxy",
        proxy_mode: "Proxy Mode",
        clash_mode: "Clash Mode",
        profile: "Profile",
        quit: "Quit BoxPilot",
        tooltip: |status| format!("BoxPilot — {status}"),
    },
    close_dialog: CloseDialog {
        title: "Keep BoxPilot running in the tray?",
        body: "BoxPilot can stay in the system tray when its window closes, so \
               sing-box stays connected. Quit stops sing-box.",
        dont_ask_again: "Don't ask again",
        quit: "Quit",
        keep: "Keep in tray",
    },
    dialogs: Dialogs {
        import_title: "Import subscription profile?",
        tun_grant_title: "Grant TUN permission",
        tun_grant_body: "TUN mode needs network-admin permission for sing-box. BoxPilot \
                         installs a copy of sing-box to /usr/local/lib/boxpilot/ and grants \
                         it once, through the system password prompt. You'll be asked \
                         again after a sing-box update.",
        grant: "Grant",
    },
    messages: Messages {
        ready: "Ready.",
        config_missing_startup: "Config not found. Please update subscription.",
        config_missing: "Config not found. Update subscription first.",
        auto_updated: "Subscription auto-updated.",
        add_subscription_first: "Add a subscription first.",
        sing_box_not_found: |binary, path| format!("{binary} not found at {path}"),
        sing_box_too_old: |found, needed| {
            format!("sing-box {found} is too old: BoxPilot needs {needed} or newer.")
        },
        api_port_retry: "The sing-box API port was taken; retrying on another port.",
        clear_logs_failed: |e| format!("Failed to clear sing-box logs: {e}"),
        disconnect_to_clear_cache: "Disconnect first to clear the cache.",
        read_app_dir_failed: |e| format!("Failed to read app directory: {e}"),
        delete_cache_failed: |e| format!("Failed to delete cache: {e}"),
        cache_cleared: |n| {
            format!(
                "Cleared {n} cache file(s). Node selection, clash mode and group expand state reset."
            )
        },
        no_cache: "No cache files to clear.",
        url_empty: "Subscription URL is empty.",
        no_file_selected: "No file selected.",
        queued_update: |name| format!("\"{name}\" will update when the current update finishes."),
        profile_updated: |name| format!("\"{name}\" updated."),
        profile_up_to_date: |name| format!("\"{name}\" is up to date."),
        profile_failed: |name, e| format!("\"{name}\": {e}"),
        ignored_import: |reason| format!("Ignored import link: {reason}"),
        clash_mode_failed: |e| format!("Failed to switch clash mode: {e}"),
        close_connection_failed: |e| format!("Failed to close connection: {e}"),
        close_connections_failed: |e| format!("Failed to close connections: {e}"),
        sing_box_exited: "sing-box exited.",
        start_failed: |binary, config, e| {
            format!("Failed to start {binary} with config {config}: {e}")
        },
        api_no_response: "sing-box API did not respond",
        groups_failed: |e| format!("Failed to load proxy groups: {e}"),
        switch_node_failed: |e| format!("Failed to switch node: {e}"),
        save_group_state_failed: |e| format!("Failed to save group state: {e}"),
        delay_test_failed: |e| format!("Delay test failed: {e}"),
        settings_backed_up: |backup| {
            format!(
                "Settings file was unreadable and has been backed up to {backup}. Started with default settings."
            )
        },
        settings_unreadable: |path, e, backup_err| {
            format!(
                "Settings file {path} is unreadable ({e}) and could not be backed up ({backup_err}). \
                 Started with default settings; changes won't be saved this session."
            )
        },
        settings_read_failed: |path, e| {
            format!(
                "Couldn't read settings file {path} ({e}). Started with default settings; \
                 changes won't be saved until BoxPilot is restarted."
            )
        },
    },
    errors: Errors {
        read_failed: |path, e| format!("Failed to read {path}: {e}"),
        write_failed: |path, e| format!("Failed to write {path}: {e}"),
        write_config: |path, e| format!("Failed to write config ({path}): {e}"),
        create_failed: |path, e| format!("Failed to create {path}: {e}"),
        parse_config: |e| format!("Failed to parse config JSON: {e}"),
        not_object: "Config is not a JSON object",
        serialize_config: |e| format!("Failed to serialize config: {e}"),
        api_port: |e| format!("Failed to find a free port for the sing-box API: {e}"),
        invalid_sub_url: "Invalid URL: must start with http:// or https://",
        http_client: |e| format!("Failed to create HTTP client: {e}"),
        update_timed_out: "Update timed out. Please try again.",
        network_error: |e| format!("Network error fetching subscription: {e}"),
        download_status: |status| format!("Failed to download subscription. Status: {status}"),
        read_response: |e| format!("Failed to read subscription response: {e}"),
        file_not_found: |path| format!("File not found: {path}"),
        not_a_file: |path| format!("Not a file: {path}"),
        validation_temp: |e| format!("Failed to write validation temp file: {e}"),
        validation_failed: |summary| format!("Config validation failed: {summary}"),
        check_run_failed: |e| format!("Failed to run sing-box check: {e}"),
        killed_by_signal: |signal| format!("sing-box was killed by signal {signal}."),
        exited_with_code: |code| format!("sing-box exited with code {code}."),
        flush_dns_ok: "Successfully flushed the DNS resolver cache.",
        flush_dns_failed: |e| format!("Failed to flush DNS cache. Error: {e}"),
        flush_dns_run: |e| format!("Failed to execute 'ipconfig /flushdns': {e}"),
        run_command: |program, e| format!("Failed to run {program}: {e}"),
        command_failed: |program, e| format!("{program} failed: {e}"),
        disable_proxy: |e| format!("Failed to disable system proxy: {e}"),
        pkexec_missing: "pkexec not found. Install polkit to grant TUN permission.",
        pkexec_run: |e| format!("Failed to run pkexec: {e}"),
        tun_dismissed: "TUN permission was not granted: the password prompt was dismissed.",
        tun_not_authorized: "TUN permission was not granted: not authorized.",
        tun_failed: |detail| format!("Failed to grant TUN permission: {detail}"),
        tun_failed_code: |code| format!("Failed to grant TUN permission (exit code {code})."),
        tun_terminated: "Failed to grant TUN permission: pkexec was terminated.",
        resolve_dir: "Failed to resolve config or current directory",
        create_app_dir: |e| format!("Failed to create app data directory: {e}"),
        exe_path: |e| format!("Failed to get current executable path: {e}"),
        exe_dir: "Failed to get executable directory",
        unsupported_scheme: "unsupported URL scheme",
        unsupported_action: |action| format!("unsupported action \"{action}\""),
        missing_url: "missing url parameter",
        profile_url_scheme: "profile URL must be http:// or https://",
        api_unreachable: |reason| format!("sing-box API unreachable: {reason}"),
        api_timed_out: "sing-box API timed out",
        api_stream: |reason| format!("sing-box API stream {reason}"),
        api_error: |message| format!("sing-box API: {message}"),
        api_invalid_response: |reason| format!("Invalid sing-box API response: {reason}"),
    },
};
