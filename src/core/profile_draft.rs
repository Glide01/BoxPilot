//! The profile dialog's draft model: the raw field text as typed, and the one
//! owner of how it becomes a `(name, ProfileSource)` — trimming, interval
//! parsing, kind selection, and the has-content gate all live here instead of
//! inside the dialog's `on_ok` closure. No gpui dependency.

use crate::core::settings::{default_auto_update_interval, Profile, ProfileSource};
use std::path::Path;

/// Which source type the dialog is editing. Index-mapped to the dialog's
/// Subscription / Local file `TabBar`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftKind {
    Remote,
    Local,
}

impl DraftKind {
    /// TabBar tab index → kind (0 = Subscription, 1 = Local file). Anything
    /// out of range falls back to `Remote`, the dialog's default tab.
    pub fn from_index(index: usize) -> Self {
        if index == 1 {
            DraftKind::Local
        } else {
            DraftKind::Remote
        }
    }

    pub fn index(self) -> usize {
        match self {
            DraftKind::Remote => 0,
            DraftKind::Local => 1,
        }
    }
}

/// Raw dialog fields. Seeded from an existing profile (Edit) or defaults
/// (Add) by [`ProfileDraft::from_profile`]; turned back into a validated
/// `(name, source)` by [`ProfileDraft::build`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileDraft {
    pub name: String,
    pub kind: DraftKind,
    pub url: String,
    /// Interval field as typed; parse failures mean 0 (= auto-update off).
    pub interval_raw: String,
    pub path: String,
}

/// `ProfileDraft::build` output: what `AppState` persists, plus the gate for
/// the immediate fetch after a create.
#[derive(Clone, Debug, PartialEq)]
pub struct DraftOutput {
    /// Trimmed; may be empty — `AppState::create_profile` numbers it
    /// ("Profile N"), an edit keeps the old name.
    pub name: String,
    pub source: ProfileSource,
    /// False when the source is empty (no URL / no file) — nothing to fetch.
    pub has_content: bool,
}

impl ProfileDraft {
    /// Seed the dialog fields. `None` = Add (Remote kind, default interval);
    /// `Some` = Edit (fields mirror the profile, kind locked by the dialog).
    pub fn from_profile(profile: Option<&Profile>) -> Self {
        let kind = if profile.map(|p| p.is_local()).unwrap_or(false) {
            DraftKind::Local
        } else {
            DraftKind::Remote
        };
        Self {
            name: profile.map(|p| p.name.clone()).unwrap_or_default(),
            kind,
            url: profile
                .and_then(|p| p.remote_url())
                .unwrap_or_default()
                .to_string(),
            interval_raw: profile
                .map(|p| p.auto_update_interval())
                .unwrap_or_else(default_auto_update_interval)
                .to_string(),
            path: profile
                .and_then(|p| p.local_path())
                .unwrap_or_default()
                .to_string(),
        }
    }

    /// The draft → model rule: only the fields of the selected kind count,
    /// everything is trimmed, and an unparseable interval means 0 (off).
    pub fn build(&self) -> DraftOutput {
        let source = match self.kind {
            DraftKind::Local => ProfileSource::Local {
                path: self.path.trim().to_string(),
            },
            DraftKind::Remote => ProfileSource::Remote {
                url: self.url.trim().to_string(),
                auto_update_interval_minutes: self.interval_raw.trim().parse().unwrap_or(0),
            },
        };
        DraftOutput {
            name: self.name.trim().to_string(),
            has_content: !source.is_empty_source(),
            source,
        }
    }
}

/// The file-browse rule: only `.json` configs are importable (gpui's native
/// dialog has no extension filter, so the pick is validated after the fact).
pub fn is_json_config(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("json"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_profile(url: &str, interval: u64) -> Profile {
        Profile {
            id: "p1".into(),
            name: "Sub".into(),
            source: ProfileSource::Remote {
                url: url.into(),
                auto_update_interval_minutes: interval,
            },
            last_updated_secs: None,
        }
    }

    #[test]
    fn add_draft_defaults_to_remote_with_default_interval() {
        let draft = ProfileDraft::from_profile(None);
        assert_eq!(draft.kind, DraftKind::Remote);
        assert_eq!(draft.interval_raw, default_auto_update_interval().to_string());
        assert!(draft.name.is_empty() && draft.url.is_empty() && draft.path.is_empty());
    }

    #[test]
    fn edit_draft_mirrors_the_profile() {
        let profile = Profile {
            id: "p2".into(),
            name: "Lab".into(),
            source: ProfileSource::Local {
                path: "C:\\box.json".into(),
            },
            last_updated_secs: None,
        };
        let draft = ProfileDraft::from_profile(Some(&profile));
        assert_eq!(draft.kind, DraftKind::Local);
        assert_eq!(draft.name, "Lab");
        assert_eq!(draft.path, "C:\\box.json");
        assert_eq!(draft.interval_raw, "0", "Local never auto-updates");
    }

    #[test]
    fn build_trims_and_parses_interval() {
        let mut draft = ProfileDraft::from_profile(None);
        draft.name = "  My Sub  ".into();
        draft.url = " https://a/s ".into();
        draft.interval_raw = " 30 ".into();
        let out = draft.build();
        assert_eq!(out.name, "My Sub");
        assert_eq!(
            out.source,
            ProfileSource::Remote {
                url: "https://a/s".into(),
                auto_update_interval_minutes: 30,
            }
        );
        assert!(out.has_content);
    }

    #[test]
    fn unparseable_interval_means_off() {
        for raw in ["", "abc", "-5", "1.5"] {
            let mut draft = ProfileDraft::from_profile(None);
            draft.url = "https://a/s".into();
            draft.interval_raw = raw.into();
            let ProfileSource::Remote {
                auto_update_interval_minutes,
                ..
            } = draft.build().source
            else {
                panic!("remote draft must build a Remote source");
            };
            assert_eq!(auto_update_interval_minutes, 0, "raw: {:?}", raw);
        }
    }

    /// Only the selected kind's fields count — a draft that toggled from
    /// Remote (with a URL typed) to Local must not leak the URL.
    #[test]
    fn build_uses_only_the_selected_kind() {
        let mut draft = ProfileDraft::from_profile(Some(&remote_profile("https://a/s", 60)));
        draft.kind = DraftKind::Local;
        draft.path = "/tmp/c.json".into();
        assert_eq!(
            draft.build().source,
            ProfileSource::Local {
                path: "/tmp/c.json".into()
            }
        );
    }

    #[test]
    fn empty_source_has_no_content() {
        let draft = ProfileDraft::from_profile(None);
        let out = draft.build();
        assert!(!out.has_content);
        assert!(out.source.is_empty_source());

        let mut local = ProfileDraft::from_profile(None);
        local.kind = DraftKind::Local;
        local.path = "   ".into();
        assert!(!local.build().has_content);
    }

    #[test]
    fn kind_round_trips_through_tab_index() {
        for kind in [DraftKind::Remote, DraftKind::Local] {
            assert_eq!(DraftKind::from_index(kind.index()), kind);
        }
        assert_eq!(DraftKind::from_index(99), DraftKind::Remote);
    }

    #[test]
    fn json_extension_check_is_case_insensitive_and_strict() {
        assert!(is_json_config(Path::new("a/config.json")));
        assert!(is_json_config(Path::new("a/CONFIG.JSON")));
        assert!(!is_json_config(Path::new("a/config.yaml")));
        assert!(!is_json_config(Path::new("a/json")));
        assert!(!is_json_config(Path::new("a/config.json.bak")));
    }
}
