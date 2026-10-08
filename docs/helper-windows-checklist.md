# Privileged helper: Windows verification checklist

The Windows helper (ADR 0006, phase 1) has been reviewed, and CI installs
and drives it on a Windows Server runner (below), but it has not run on a
Windows client SKU or under the GUI. Run this on clean
Windows 10 and Windows 11 machines, with a standard account, an
unelevated administrator and an elevated administrator, before the first
release that ships it. Each item names what must hold.

**CI covers part of it.** The release workflow's `windows` job installs
the MSI it just built on GitHub's `windows-latest` runner and drives the
service from outside with a smoke client
(`crates/boxpilot-helper/examples/service_smoke.rs`), as the runner's
elevated administrator and as a fresh standard account, through
`packaging/windows/helper-smoke.ps1` (one function per check). Items or
parts marked **CI** are checked there. That runner is Windows Server, not
the Windows 10 and 11 client SKUs this list is for, and has no unelevated
administrator, Network Configuration Operators member or GUI session, so
a release still needs the manual run; what is unmarked is manual only.

## Install, upgrade, uninstall (MSI)

- `sc qc BoxPilotHelper`: quoted image path in
  `C:\Program Files\BoxPilot\Helper\`, demand start, LocalSystem. **CI**
- `sc sdshow BoxPilotHelper`: the MSI's SDDL (**CI**, compared ACE by ACE
  with `wix/main.wxs`). A standard user can start the service but not
  stop it, change its config or its descriptor (**CI**: `sc start`,
  `stop`, `config` and `sdset` as the standard account).
- `icacls "C:\Program Files\BoxPilot\HelperState"`: owner SYSTEM,
  protected, SYSTEM and Administrators only (**CI**, right after the
  install and again, for everything in it, once the helper has used it);
  Users can't even list it (**CI**: listing it and opening `helper.log`
  as the standard account fail as access denied).
- `C:\Program Files\BoxPilot\Helper` holds exactly `boxpilot-helper.exe`,
  `sing-box.exe`, `libcronet.dll`, `manifest.json` (**CI**, with the
  manifest's hashes), and passes the helper's own check (**CI**: the
  helper starts at all; CI prints `icacls` of the whole chain from `C:\`
  so the real Program Files ACLs can be compared with the test fixtures,
  which were written from knowledge, not measured).
- `current_exe()` is the plain Program Files path (no `PROGRA~1`, no
  `\\?\` prefix). **CI**, implicitly: the helper refuses any other with
  exit code 10, and it starts.
- Upgrade over a release from before the helper; then helper to helper.
- Uninstall: the service is gone; `HelperState` stays once used. **CI**

## Service lifecycle

- Starts on demand from the GUI (**CI** from the smoke client, which
  connects as the GUI does), reaches RUNNING, exits after 60 s idle with
  code 0 (**CI**), stops cleanly on `sc stop` (**CI**) and at shutdown.
- A broken install reports its service-specific exit code
  (`boxpilot_protocol::endpoint::exit`), and the GUI shows the matching
  message: a tampered `sing-box.exe` (manifest; **CI**), a Users write ACE
  on the helper directory (**CI**) or `sing-box.exe` (helper directory;
  **CI** checks that the helper refuses to run, and warns if the code
  isn't the documented one), an inherited Users read on `HelperState`
  (state directory; **CI** with an explicit Users read ACE, which the
  helper judges the same way), a junction anywhere in either chain. The
  GUI's message is manual.
- With the name pre-created by an administrator's process, the helper
  exits with the squatted-pipe code. **CI**

## Pipe and authority

- As SYSTEM, the ProtectedPrefix pipe is created with
  `FILE_FLAG_FIRST_PIPE_INSTANCE` (**CI**, by the squatted-name check). A
  standard user can neither create the name nor add an instance of it.
  **CI**
- A standard user (**CI**) and an unelevated admin can open it with
  `GENERIC_READ | FILE_WRITE_DATA`; `GENERIC_WRITE` is denied (**CI**, for
  the standard user); a remote client is refused; a low-integrity process
  can't write.
- `ImpersonateNamedPipeClient` works right after the connect, before the
  client's first write, including for identification-level clients
  (**CI**, implicitly: the smoke client connects with an
  identification-only QoS, and an administrator is told it may start); an
  anonymous client is read-only.
- `GetTokenInformation(TokenRestrictedSids)` on a normal token is read as
  zero restricted SIDs (if it fails, every caller would be read-only).
  **CI**, implicitly, as above.
- Elevated (**CI**) and unelevated administrators may start; an
  unelevated Network Configuration Operators member shows S-1-5-32-556 as
  deny-only in `whoami /groups` and may start; a standard user is
  read-only (**CI**: `hello`, and `start` and `stop` refused as
  unauthorized).
- Four read-only connections held by a standard account don't stop an
  administrator from starting; a fifth read-only connection is closed.
  **CI** (the administrator's `hello` is served meanwhile)

## sing-box under the helper

- `CreateProcessAsUserW` succeeds while `sing-box.exe` and
  `libcronet.dll` are held open sharing reads only (**CI**: every TUN
  start); an overwrite attempt during the spawn fails.
- sing-box's token (Process Explorer's Security tab, or
  `service_smoke token --pid <sing-box> --expect sing-box` from an
  elevated prompt): SYSTEM, `SeChangeNotifyPrivilege` and no other
  privilege, integrity High, Administrators enabled
  (`tokenplan::SING_BOX_TOKEN`). **CI**
- The helper's own token (`--expect helper`): SYSTEM, only
  `SeChangeNotifyPrivilege` and `SeLoadDriverPrivilege`, integrity System
  (`tokenplan::HELPER_TOKEN`), also after an administrator ran
  `sc.exe privs BoxPilotHelper` with every privilege. **CI**
- With only those tokens, the machine's first TUN start installs wintun's
  driver, and later ones work (**CI**, on Windows Server 2025, by the
  token probe and every TUN run). Confirm it on Windows 10 and 11 with the
  token probe, below.
- Process Explorer: sing-box inherits only its two output pipes, has the
  minimal environment, and the three mitigations (no remote images, no
  low-label images, extension points disabled). Windows 10 releases older
  than 1511 don't reject those bits. (**CI** checks only that sing-box is
  the helper's child, runs the installed binary, in a run directory under
  `HelperState`.)
- Inside the job (one process), wintun installs its driver and creates
  the adapter (**CI**); auto_route (**CI**: this machine's own
  connections start from the TUN address), strict_route (on in CI's
  runs, not checked by itself) and DNS (**CI**: a name resolves through
  TUN with the profile's DNS hijacked) work; a naive outbound loads
  `libcronet.dll`; Tailscale endpoints work with the scrubbed
  environment.
- Killing the helper kills sing-box. **CI**
- Adapters: after sing-box stops, its adapter reads as no longer present
  and is removed (**CI**: none is present after a run; one left installed
  is reported as a warning); another program's live `sing-tun` adapter
  survives a helper start.
- The loopback rule: through the local proxy (**CI**: SOCKS5 and HTTP
  CONNECT to `127.0.0.1`, `127.1.2.3`, `::1`, `localhost` and a name under
  it never reach a listener there) and through TUN, `127.0.0.1` and
  `localhost` are rejected; normal browsing is not affected (**CI**: the
  proxy and TUN reach the internet by address and by name).
- Taildrop names `..\x`, `C:x`, `a:b` and `NUL` are refused by sing-box.

### The token probe, by hand

The probe CI runs (`crates/boxpilot-helper/examples/token_probe.rs`)
measures which token sing-box needs on the machine it runs on, and checks
the shipped plans. On a clean Windows 10 or 11 machine with BoxPilot's MSI
installed, from an elevated PowerShell in the repository (Rust with the
MSVC toolchain):

    cargo build --release -p boxpilot-helper --example token_probe --example service_smoke
    packaging/windows/helper-smoke.ps1 -Step token-probe `
        -Smoke target/release/examples/service_smoke.exe `
        -Probe target/release/examples/token_probe.exe

It runs the probe as SYSTEM through a one-shot scheduled task from a
folder only SYSTEM and Administrators may write, prints its trials and
summary (also in `C:\boxpilot-token-probe\summary.txt`), then checks the
helper's own token. It holds when its summary says "regression check:
held" and the step ends without an error. Do this before anything else on
the machine brings TUN up, so its first trial is the machine's first
wintun install; note the Windows build in the result. It leaves
`BoxPilotHelper` requiring every privilege SYSTEM holds (`sc.exe qprivs`),
which gives it what no list gives.

It removes wintun's driver package from the driver store and installs it
again. On a machine where another program uses wintun (WireGuard, another
sing-box client), whose package that would be too, run the probe alone
with `--keep-driver` instead, as SYSTEM, and read its summary:

    New-Item -ItemType Directory C:\boxpilot-token-probe
    icacls C:\boxpilot-token-probe /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F'
    Copy-Item target\release\examples\token_probe.exe C:\boxpilot-token-probe
    schtasks /create /tn BoxPilotTokenProbe /ru SYSTEM /sc once /st 00:00 /f `
        /tr "C:\boxpilot-token-probe\token_probe.exe --work C:\boxpilot-token-probe --keep-driver"
    schtasks /run /tn BoxPilotTokenProbe
    # when C:\boxpilot-token-probe\done exists:
    Get-Content C:\boxpilot-token-probe\summary.txt
    schtasks /delete /tn BoxPilotTokenProbe /f

The folder must stay private: the task runs what is in it as SYSTEM, and
a folder made under `C:\` otherwise lets users write in it. With
`--keep-driver` no first install is tried after the first trial, so the
summary says the shipped plan's first install is untested.

## Connections, deadlines, cleanup

- Deadlines and cancellation: a client vanishing mid-frame stops its
  sing-box (**CI**); a client that stops reading is dropped at the write
  deadline (**CI**); the error reply is still readable after the helper
  closes (**CI**: the standard account's refused start); the 9th client
  waits and is then served (**CI**).
- Run directories are removed after each run (**CI**); the helper log
  rotates at 1 MiB.
- The console seam (`--console --root <dir>`) runs as a standard user or
  under `runas /trustlevel:0x20000`, and refuses to run elevated.

## GUI

The GUI isn't in CI: the smoke client speaks the GUI's protocol, but
isn't the GUI.

- Starting BoxPilot shows no UAC prompt. Proxy mode works. TUN works
  when the user runs BoxPilot as Administrator themselves (sing-box runs
  directly, as written).
- TUN through the helper: the service starts on demand; Logs, Groups,
  Connections and Traffic work on the helper's API; a profile with a
  local `.srs` rule set or CA certificate gets through as attachments
  (the helper's half is in CI: the smoke profile's local rule set travels
  as an attachment, and sing-box starts on it); a profile with tor or
  `executable_path` shows the refusal naming the field (the helper's
  `refused` reply is in CI); a restart after a settings change survives
  the helper's idle exit.
- Stop: sing-box exits, the helper goes idle.
- Killing BoxPilot in Task Manager while TUN runs stops sing-box.
- System proxy in helper TUN mode is set and cleared by the GUI; a proxy
  the user set themselves is left alone; running browsers pick it up.
- Deep links from a browser reach an unelevated primary and one the user
  elevated themselves; a plain second launch surfaces the window.
- Helper missing (portable exe), disabled, or a standard account outside
  Administrators and Network Configuration Operators: each gets its own
  clear message, and nothing elevates.
