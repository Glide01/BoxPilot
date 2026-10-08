//! One start's private run directory: its config, its attachments, and
//! sing-box's `-D` working directory (ADR 0006 rule 2, "Files travel as
//! content").
//!
//! The platform creates it fresh, under a random name, readable and
//! writable by SYSTEM and Administrators only, and hands it over here. Every
//! file is created new (`create_new`: `O_EXCL` / `CREATE_NEW`), so nothing
//! already at a name, a link least of all, is ever followed or reused; and
//! every name is one the helper derived, never one a caller sent. When the
//! run is over, the whole directory goes.

#![forbid(unsafe_code)]

use crate::runcfg::{Prepared, CONFIG_FILE};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// A run directory, removed with everything in it when dropped.
#[derive(Debug)]
pub struct RunDir {
    path: PathBuf,
    system_proxy: Option<u16>,
}

impl RunDir {
    /// Take charge of `path`: a directory the platform has just created,
    /// empty and private.
    pub fn adopt(path: PathBuf) -> Self {
        Self {
            path,
            system_proxy: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where the config sing-box runs is.
    pub fn config_path(&self) -> PathBuf {
        self.path.join(CONFIG_FILE)
    }

    /// Write the run's config and attachments, each as a new file, and
    /// remember what the config changes outside the run directory
    /// ([`RunDir::system_proxy_port`]).
    pub fn write(&mut self, prepared: &Prepared) -> io::Result<()> {
        self.create_file(CONFIG_FILE, prepared.config().as_bytes())?;
        for (name, content) in prepared.files() {
            self.create_file(name, content)?;
        }
        self.system_proxy = prepared.system_proxy_port();
        Ok(())
    }

    /// The written config's `Prepared::system_proxy_port`: the OS proxy
    /// setting this run's sing-box points at `127.0.0.1` and this port,
    /// which the platform resets if sing-box exits without undoing it.
    pub fn system_proxy_port(&self) -> Option<u16> {
        self.system_proxy
    }

    /// Create a directory in the run directory, for sing-box's `TEMP` and
    /// profile directory.
    pub fn create_dir(&self, name: &str) -> io::Result<PathBuf> {
        let path = self.child(name)?;
        fs::create_dir(&path)?;
        Ok(path)
    }

    fn create_file(&self, name: &str, content: &[u8]) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.child(name)?)?;
        file.write_all(content)?;
        file.sync_all()
    }

    /// `name` in the run directory, if it is a plain name: no separator, no
    /// `..`, no drive or stream.
    fn child(&self, name: &str) -> io::Result<PathBuf> {
        let plain = !name.is_empty()
            && name != "."
            && name != ".."
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
        if !plain {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a plain file name",
            ));
        }
        Ok(self.path.join(name))
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        // Best effort: whatever is left is removed with the rest of the runs
        // directory when the helper next starts.
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runcfg::{build, check, SystemProxy};
    use crate::testing::TempDir;
    use boxpilot_policy::Placement;
    use boxpilot_protocol::{StartRequest, TunOptions};
    use serde_json::json;
    use std::io;

    fn prepared(run_dir: &Path) -> crate::runcfg::Prepared {
        prepared_with(run_dir, false)
    }

    fn prepared_with(run_dir: &Path, system_proxy: bool) -> crate::runcfg::Prepared {
        let start = StartRequest {
            config: json!({
                "route": {"rule_set": [{
                    "type": "local", "tag": "geo", "format": "binary",
                    "path": "boxpilot-attachment:geo"
                }]}
            })
            .to_string(),
            attachments: vec![
                ("geo".into(), b"rules".to_vec()),
                ("unused".into(), b"not referenced".to_vec()),
            ],
            options: TunOptions {
                ipv6: false,
                proxy_port: 7890,
                allow_lan: false,
                system_proxy,
            },
        };
        let placement = Placement {
            run_dir: run_dir.to_str().unwrap().into(),
            cache_file: "cache.db".into(),
            tailscale_dir: "tailscale".into(),
            separator: std::path::MAIN_SEPARATOR,
        };
        build(
            check(start).unwrap(),
            &placement,
            SystemProxy::AsRequested,
            || Ok((41234, ())),
            &[7; 32],
        )
        .unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn attachments_land_only_in_the_run_directory() {
        let temp = TempDir::new("rundir");
        let path = temp.0.join("run");
        fs::create_dir(&path).unwrap();
        let mut run = RunDir::adopt(path.clone());
        let prepared = prepared(&path);
        run.write(&prepared).unwrap();
        assert_eq!(names(&temp.0), ["run"]);
        assert_eq!(names(&path), ["attachment-67656f", "config.json"]);
        assert_eq!(fs::read(path.join("attachment-67656f")).unwrap(), b"rules");
        assert_eq!(
            fs::read_to_string(run.config_path()).unwrap(),
            prepared.config()
        );
        // The config points where the file is.
        let config: serde_json::Value = serde_json::from_str(prepared.config()).unwrap();
        assert_eq!(
            config["route"]["rule_set"][0]["path"],
            json!(path.join("attachment-67656f").to_str().unwrap())
        );
        assert_eq!(run.system_proxy_port(), None);
        drop(run);
        assert!(!path.exists(), "the run directory goes with the run");
        assert_eq!(names(&temp.0), Vec::<String>::new());
    }

    /// What the config sets outside the run directory travels with it, for
    /// the platform to undo if sing-box can't.
    #[test]
    fn the_system_proxy_port_is_remembered() {
        let temp = TempDir::new("rundir-proxy");
        let path = temp.0.join("run");
        fs::create_dir(&path).unwrap();
        let mut run = RunDir::adopt(path.clone());
        assert_eq!(run.system_proxy_port(), None);
        run.write(&prepared_with(&path, true)).unwrap();
        assert_eq!(run.system_proxy_port(), Some(7890));
    }

    #[test]
    fn nothing_already_there_is_reused() {
        let temp = TempDir::new("rundir-exists");
        let path = temp.0.join("run");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("config.json"), "planted").unwrap();
        let mut run = RunDir::adopt(path.clone());
        let error = run.write(&prepared(&path)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(path.join("config.json")).unwrap(),
            "planted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_link_is_never_followed() {
        let temp = TempDir::new("rundir-link");
        let path = temp.0.join("run");
        fs::create_dir(&path).unwrap();
        let target = temp.0.join("target");
        std::os::unix::fs::symlink(&target, path.join("config.json")).unwrap();
        let mut run = RunDir::adopt(path.clone());
        assert_eq!(
            run.write(&prepared(&path)).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(!target.exists());
    }

    #[test]
    fn only_plain_names_are_created() {
        let temp = TempDir::new("rundir-names");
        let run = RunDir::adopt(temp.0.join("run"));
        fs::create_dir(run.path()).unwrap();
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", "C:x", "x:stream"] {
            assert_eq!(
                run.create_dir(bad).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{bad:?}"
            );
        }
        assert_eq!(run.create_dir("tmp").unwrap(), run.path().join("tmp"));
        assert!(run.path().join("tmp").is_dir());
    }
}
