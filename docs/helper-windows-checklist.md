# Privileged helper: Windows verification checklist

The Windows helper (ADR 0006, phase 1) was type-checked and unit-tested on
Linux and reviewed, but none of it has run on Windows. Run this on clean
Windows 10 and Windows 11 machines, with a standard account, an
unelevated administrator and an elevated administrator, before the first
release that ships it. Each item names what must hold.

## Install, upgrade, uninstall (MSI)

- `sc qc BoxPilotHelper`: quoted image path in
  `C:\Program Files\BoxPilot\Helper\`, demand start, LocalSystem.
- `sc sdshow BoxPilotHelper`: the MSI's SDDL. A standard user can start
  the service but not stop it, change its config or its descriptor.
- `icacls "C:\Program Files\BoxPilot\HelperState"`: owner SYSTEM,
  protected, SYSTEM and Administrators only; Users can't even list it.
- `C:\Program Files\BoxPilot\Helper` holds exactly `boxpilot-helper.exe`,
  `sing-box.exe`, `libcronet.dll`, `manifest.json`, and passes the
  helper's own check (the real Program Files ACLs match the test
  fixtures, which were written from knowledge, not measured).
- `current_exe()` is the plain Program Files path (no `PROGRA~1`, no
  `\\?\` prefix).
- Upgrade over a release from before the helper; then helper to helper.
- Uninstall: the service is gone; `HelperState` stays once used.

## Service lifecycle

- Starts on demand from the GUI, reaches RUNNING, exits after 60 s idle
  with code 0, stops cleanly on `sc stop` and at shutdown.
- A broken install reports its service-specific exit code
  (`boxpilot_protocol::endpoint::exit`), and the GUI shows the matching
  message: a tampered `sing-box.exe` (manifest), a Users write ACE on the
  helper directory or `sing-box.exe` (helper directory), an inherited
  Users read on `HelperState` (state directory), a junction anywhere in
  either chain.
- With the name pre-created by an administrator's process, the helper
  exits with the squatted-pipe code.

## Pipe and authority

- As SYSTEM, the ProtectedPrefix pipe is created with
  `FILE_FLAG_FIRST_PIPE_INSTANCE`. A standard user can neither create the
  name nor add an instance of it.
- A standard user and an unelevated admin can open it with
  `GENERIC_READ | FILE_WRITE_DATA`; `GENERIC_WRITE` is denied; a remote
  client is refused; a low-integrity process can't write.
- `ImpersonateNamedPipeClient` works right after the connect, before the
  client's first write, including for identification-level clients; an
  anonymous client is read-only.
- `GetTokenInformation(TokenRestrictedSids)` on a normal token is read as
  zero restricted SIDs (if it fails, every caller would be read-only).
- Elevated and unelevated administrators may start; an unelevated
  Network Configuration Operators member shows S-1-5-32-556 as deny-only
  in `whoami /groups` and may start; a standard user is read-only.
- Four read-only connections held by a standard account don't stop an
  administrator from starting; a fifth read-only connection is closed.

## sing-box under the helper

- `CreateProcessW` succeeds while `sing-box.exe` and `libcronet.dll` are
  held open sharing reads only; an overwrite attempt during the spawn
  fails.
- Process Explorer: sing-box inherits only its two output pipes, has the
  minimal environment, and the three mitigations (no remote images, no
  low-label images, extension points disabled). Windows 10 releases older
  than 1511 don't reject those bits.
- Inside the job (one process), wintun installs its driver and creates
  the adapter; auto_route, strict_route and DNS work; a naive outbound
  loads `libcronet.dll`; Tailscale endpoints work with the scrubbed
  environment.
- Killing the helper kills sing-box.
- Adapters: after sing-box stops, its adapter reads as no longer present
  and is removed; another program's live `sing-tun` adapter survives a
  helper start.
- The loopback rule: through the local proxy and through TUN, `127.0.0.1`
  and `localhost` are rejected; normal browsing is not affected.
- Taildrop names `..\x`, `C:x`, `a:b` and `NUL` are refused by sing-box.

## Connections, deadlines, cleanup

- Deadlines and cancellation: a client vanishing mid-frame stops its
  sing-box; a client that stops reading is dropped at the write
  deadline; the error reply is still readable after the helper closes;
  the 9th client waits and is then served.
- Run directories are removed after each run; the helper log rotates at
  1 MiB.
- The console seam (`--console --root <dir>`) runs as a standard user or
  under `runas /trustlevel:0x20000`, and refuses to run elevated.

## GUI

- Starting BoxPilot shows no UAC prompt. Proxy mode works. TUN works
  when the user runs BoxPilot as Administrator themselves (sing-box runs
  directly, as written).
- TUN through the helper: the service starts on demand; Logs, Groups,
  Connections and Traffic work on the helper's API; a profile with a
  local `.srs` rule set or CA certificate gets through as attachments; a
  profile with tor or `executable_path` shows the refusal naming the
  field; a restart after a settings change survives the helper's idle
  exit.
- Stop: sing-box exits, the helper goes idle.
- Killing BoxPilot in Task Manager while TUN runs stops sing-box.
- System proxy in helper TUN mode is set and cleared by the GUI; a proxy
  the user set themselves is left alone; running browsers pick it up.
- Deep links from a browser reach an unelevated primary and one the user
  elevated themselves; a plain second launch surfaces the window.
- Helper missing (portable exe), disabled, or a standard account outside
  Administrators and Network Configuration Operators: each gets its own
  clear message, and nothing elevates.
