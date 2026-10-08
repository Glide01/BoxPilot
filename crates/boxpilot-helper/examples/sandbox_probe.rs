//! `sandbox_probe`: sing-box's sandbox profile, checked for what it denies
//! (ADR 0006, "Defense in depth"; `sandboxplan`), as `token_probe` checks
//! sing-box's token on Windows. CI's macOS job runs it as root through
//! `packaging/macos/helper-smoke.sh sandbox-probe`; it fails the job if a
//! denial the profile promises doesn't hold, or something sing-box needs is
//! denied. It is a test tool: nothing installs it.
//!
//! As root, it builds a tree of its own shaped like the helper's (`bin`,
//! `state/runs/<run>`, `state/users/<uid>`, and another account's
//! `state/users/<uid>` with a file in it), copies itself into `bin` as the
//! one program the profile lets run, plants a file in a user's home and one
//! in root's, and runs itself through `/usr/bin/sandbox-exec` with exactly
//! the shipped profile and the parameters the helper builds
//! (`sandboxplan::Params::for_run`, `sandbox_exec_args`). Inside, as root
//! under the profile, it tries what the profile must deny (writing outside
//! its run, reading a user's home, root's, another account's state, password
//! hashes; running a shell; forking; another local service's socket), and
//! what it must still allow (its run and its account's state, the routing
//! and utun sockets, IP sockets, mDNSResponder), and says which held. The
//! tree and the planted files are removed afterwards.
//!
//!   sudo sandbox_probe --home <a user's home> [--socket <a Unix socket>]
//!
//! `--socket`: a local service's socket that exists (the helper's own, in
//! CI), which the profile must not let sing-box connect to.
//!
//! Exit code 0 when every expectation held, 1 when one didn't, 2 for a bad
//! command line or a probe that couldn't run.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("sandbox_probe checks sing-box's macOS sandbox; it runs on macOS only");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
fn main() {
    std::process::exit(probe::main());
}

#[cfg(target_os = "macos")]
mod probe {
    use boxpilot_helper::paths::{run_name, Layout};
    use boxpilot_helper::sandboxplan::{sandbox_exec_args, Params, SANDBOX_EXEC, STATUS};
    use boxpilot_helper::spawnplan::POSIX_PATH;
    use std::fs::{self, OpenOptions};
    use std::io::{self, Write};
    use std::net::{Ipv4Addr, UdpSocket};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    const HELD: i32 = 0;
    const FAILED: i32 = 1;
    const USAGE_ERROR: i32 = 2;

    /// What the inside says first, so the outside knows it ran sandboxed.
    const INSIDE: &str = "inside the sandbox, as uid";

    /// The probe's own account, and another one whose state it must not
    /// read.
    const OWN_UID: &str = "4242";
    const OTHER_UID: &str = "4243";

    const USAGE: &str = "\
usage: sudo sandbox_probe --home <a user's home> [--socket <a Unix socket>]";

    pub fn main() -> i32 {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.first().map(String::as_str) == Some("--inside") {
            return inside(&args[1..]);
        }
        let mut home = None;
        let mut socket = None;
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            match (arg.as_str(), rest.next()) {
                ("--home", Some(value)) => home = Some(PathBuf::from(value)),
                ("--socket", Some(value)) => socket = Some(PathBuf::from(value)),
                _ => {
                    eprintln!("{USAGE}");
                    return USAGE_ERROR;
                }
            }
        }
        let Some(home) = home else {
            eprintln!("{USAGE}");
            return USAGE_ERROR;
        };
        // SAFETY: geteuid has no preconditions and cannot fail.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("sandbox_probe: run it as root: sing-box runs as root");
            return USAGE_ERROR;
        }
        match outside(&home, socket.as_deref()) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("sandbox_probe: FAILED: {error}");
                USAGE_ERROR
            }
        }
    }

    /// The tree, the planted files, then the inside, through sandbox-exec.
    fn outside(home: &Path, socket: Option<&Path>) -> Result<i32, String> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|error| error.to_string())?;
        let base = PathBuf::from(format!(
            "/private/var/tmp/boxpilot-sandbox-probe-{}",
            &run_name(&random)[..8]
        ));
        let home_file = home.join("boxpilot-sandbox-probe.txt");
        let root_file = PathBuf::from("/private/var/root/boxpilot-sandbox-probe.txt");
        let result = (|| {
            let layout = Layout::new(base.join("bin"), base.join("state"));
            let run = layout.run_dir(&run_name(&random));
            let own = layout.user_dir(OWN_UID).expect("a uid");
            let other = layout.user_dir(OTHER_UID).expect("a uid");
            for (dir, mode) in [
                (base.as_path(), 0o755),
                (layout.helper_dir(), 0o755),
                (layout.state_dir(), 0o700),
                (layout.runs_dir().as_path(), 0o700),
                (run.as_path(), 0o700),
                (layout.users_dir().as_path(), 0o700),
                (own.as_path(), 0o700),
                (other.as_path(), 0o700),
            ] {
                fs::create_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
                fs::set_permissions(dir, fs::Permissions::from_mode(mode))
                    .map_err(|error| error.to_string())?;
            }
            let other_file = other.join("secret");
            for file in [&other_file, &home_file, &root_file] {
                fs::write(file, "a secret\n")
                    .map_err(|error| format!("{}: {error}", file.display()))?;
            }
            let program = layout.helper_file("sandbox-probe");
            let exe = std::env::current_exe().map_err(|error| error.to_string())?;
            fs::copy(&exe, &program).map_err(|error| format!("copying itself: {error}"))?;
            fs::set_permissions(&program, fs::Permissions::from_mode(0o755))
                .map_err(|error| error.to_string())?;

            let params = Params::for_run(&layout, &program, &run, &own)
                .map_err(|error| error.to_string())?;
            let text = |path: &Path| path.to_str().expect("a plain path").to_owned();
            let mut inner = vec![
                "--inside".to_owned(),
                text(&run),
                text(&own),
                text(layout.state_dir()),
                text(&other_file),
                text(&home_file),
                text(&root_file),
            ];
            if let Some(socket) = socket {
                inner.push(text(socket));
            }
            let args = sandbox_exec_args(&params, &inner);
            println!("sing-box's sandbox profile ({STATUS}), with the parameters:");
            for (name, value) in params.pairs() {
                println!("  {name}={value}");
            }
            let output = Command::new(SANDBOX_EXEC)
                .args(&args)
                .env_clear()
                .env("PATH", POSIX_PATH)
                .current_dir(&run)
                .output()
                .map_err(|error| format!("{SANDBOX_EXEC}: {error}"))?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            print!("{stdout}");
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            if !stdout.contains(INSIDE) {
                return Err(format!(
                    "the probe never ran inside the sandbox ({}); see above",
                    output.status
                ));
            }
            Ok(if output.status.success() {
                HELD
            } else {
                FAILED
            })
        })();
        let _ = fs::remove_dir_all(&base);
        for file in [&home_file, &root_file] {
            let _ = fs::remove_file(file);
        }
        result
    }

    /// Whether `error` is the sandbox's denial: `EPERM` (or `EACCES`).
    fn is_denial(error: &io::Error) -> bool {
        matches!(error.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES))
    }

    struct Checks {
        broken: usize,
    }

    impl Checks {
        /// `result` must be the sandbox's denial.
        fn denied<T>(&mut self, what: &str, result: io::Result<T>) {
            match result {
                Err(error) if is_denial(&error) => println!("held: denied {what} ({error})"),
                Err(error) => {
                    self.broken += 1;
                    println!("BROKEN: {what} failed, but not as a denial: {error}");
                }
                Ok(_) => {
                    self.broken += 1;
                    println!("BROKEN: {what} was allowed");
                }
            }
        }

        /// `result` must succeed.
        fn allowed<T>(&mut self, what: &str, result: io::Result<T>) {
            match result {
                Ok(_) => println!("held: allowed {what}"),
                Err(error) => {
                    self.broken += 1;
                    println!("BROKEN: {what} was refused: {error}");
                }
            }
        }
    }

    fn create(path: &Path) -> io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(b"written under the sandbox\n")
    }

    /// `socket(domain, kind, protocol)`, closed again.
    fn socket(domain: libc::c_int, kind: libc::c_int, protocol: libc::c_int) -> io::Result<()> {
        // SAFETY: socket takes three integers and returns a new descriptor
        // or -1.
        let fd = unsafe { libc::socket(domain, kind, protocol) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: closes the descriptor just opened, once.
        unsafe { libc::close(fd) };
        Ok(())
    }

    /// `fork`: the child exits at once; Ok means it was allowed.
    fn fork() -> io::Result<()> {
        // SAFETY: the probe has one thread here; the child only calls
        // `_exit`, which is async-signal-safe.
        let pid = unsafe { libc::fork() };
        match pid {
            -1 => Err(io::Error::last_os_error()),
            // SAFETY: the child ends at once, running nothing of the parent.
            0 => unsafe { libc::_exit(0) },
            child => {
                let mut status = 0;
                // SAFETY: waits for the child just forked.
                unsafe { libc::waitpid(child, &mut status, 0) };
                Ok(())
            }
        }
    }

    /// Inside the sandbox: `<run> <own state> <state dir> <other's file>
    /// <home file> <root's file> [<socket>]`.
    fn inside(args: &[String]) -> i32 {
        let [run, own, state, other_file, home_file, root_file, rest @ ..] = args else {
            eprintln!("sandbox_probe --inside: bad arguments {args:?}");
            return USAGE_ERROR;
        };
        // SAFETY: geteuid has no preconditions and cannot fail.
        println!("{INSIDE} {}", unsafe { libc::geteuid() });
        let mut checks = Checks { broken: 0 };
        let run = Path::new(run);
        let own = Path::new(own);
        let state = Path::new(state);

        // What sing-box must not do.
        let outside_run = Path::new("/private/tmp/boxpilot-sandbox-probe-x");
        checks.denied("writing /private/tmp", create(outside_run));
        let _ = fs::remove_file(outside_run);
        let prefs = Path::new("/Library/Preferences/boxpilot-sandbox-probe.plist");
        checks.denied("writing /Library/Preferences", create(prefs));
        let _ = fs::remove_file(prefs);
        checks.denied(
            "writing the state directory outside its run and account",
            create(&state.join("owner-probe")),
        );
        checks.denied(
            &format!("reading a user's home ({home_file})"),
            fs::read(home_file),
        );
        checks.denied(
            &format!("reading root's home ({root_file})"),
            fs::read(root_file),
        );
        checks.denied(
            &format!("reading another account's state ({other_file})"),
            fs::read(other_file),
        );
        checks.denied(
            "reading the password hashes (/private/etc/master.passwd)",
            fs::read("/private/etc/master.passwd"),
        );
        checks.denied(
            "running a shell (/bin/sh -c true)",
            Command::new("/bin/sh").args(["-c", "true"]).status(),
        );
        checks.denied("forking", fork());
        match rest.first() {
            Some(socket) => checks.denied(
                &format!("connecting to a local service's socket ({socket})"),
                UnixStream::connect(socket),
            ),
            None => println!("note: no local service's socket given: not checked"),
        }

        // What sing-box needs.
        checks.allowed(
            "writing and reading its run directory",
            create(&run.join("probe.txt")).and_then(|()| fs::read(run.join("probe.txt"))),
        );
        checks.allowed(
            "writing its account's state (cache.db)",
            create(&own.join("cache.db")),
        );
        checks.allowed(
            "a routing socket (AF_ROUTE)",
            socket(libc::AF_ROUTE, libc::SOCK_RAW, 0),
        );
        checks.allowed(
            "a kernel control socket (AF_SYSTEM, utun's)",
            socket(libc::AF_SYSTEM, libc::SOCK_DGRAM, libc::SYSPROTO_CONTROL),
        );
        checks.allowed("reading /private/etc/hosts", fs::read("/private/etc/hosts"));
        checks.allowed(
            "a UDP socket to 1.1.1.1:53",
            UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                .and_then(|socket| socket.connect((Ipv4Addr::new(1, 1, 1, 1), 53))),
        );
        checks.allowed(
            "connecting to mDNSResponder (the local DNS server)",
            UnixStream::connect("/private/var/run/mDNSResponder"),
        );

        if checks.broken == 0 {
            println!("every expectation held");
            HELD
        } else {
            println!("{} expectations didn't hold", checks.broken);
            FAILED
        }
    }
}
