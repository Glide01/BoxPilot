//! The helper's process exit codes. A service reports them to the SCM as its
//! service-specific exit code, where the GUI can read them. Defined in the
//! protocol crate (`boxpilot_protocol::endpoint::exit`), which the GUI
//! shares; re-exported here under the names the helper uses.

#![forbid(unsafe_code)]

pub use boxpilot_protocol::endpoint::exit::*;
