//! The configuration: `golem.toml`, and the per-run overrides over it.
//!
//! One file holds every setting, and it is the only file read or written:
//! [`get`] and [`set`] are the whole surface, so no reader can disagree with
//! another about where a key lives.
//!
//! A few keys may be pointed somewhere else for a single run - the tuner
//! serves a candidate preset, a container binds every interface - and that is
//! an [`Overrides`] value handed to whoever resolves the start, never a
//! change to the process environment. The `GOLEM_<KEY>` variables are how a
//! caller *spells* an override on the command line; they are read once, at
//! the edge, by [`Overrides::from_env`].

use std::path::Path;

/// One key from the home's `golem.toml`, or `None` when the file or the key
/// is not there.
pub fn get(home: &Path, key: &str) -> Option<String> {
    golem_core::config::value(home, key)
}

/// One key, with this process's `GOLEM_<KEY>` override winning - the
/// resolution a measuring command uses, whose caller (the tuner, a script)
/// names the model it should measure in the child's environment.
pub fn resolved(home: &Path, key: &str) -> Option<String> {
    Overrides::from_env().get(home, key)
}

/// Set one key in the home's `golem.toml`, keeping every comment around it.
///
/// The line that assigns the key is replaced where it stands, and a key the
/// file does not have is appended. A file that will not parse is refused
/// rather than rewritten: writing over a file nobody can read would lose
/// whatever the user meant by it.
pub fn set(home: &Path, key: &str, value: &str) -> std::io::Result<()> {
    let path = home.join("golem.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut document = golem_core::config::Document::parse(&text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    document.set(key, value);
    std::fs::write(path, document.text())
}

/// The keys a single run may point elsewhere: which engine, which preset
/// file, which section of it, and which address the engine binds.
pub const OVERRIDABLE: [&str; 4] = ["ENGINE", "MODELS_PRESET", "MODEL_ALIAS", "LOCAL_AI_HOST"];

/// One run's word on the [`OVERRIDABLE`] keys, over what `golem.toml` says.
///
/// Held as plain key names; `GOLEM_` is only the spelling at the two edges,
/// the process environment and the service's wire format.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pairs: Vec<(String, String)>,
}

impl Overrides {
    /// The `GOLEM_<KEY>` variables this process was started with. An empty
    /// value counts as unset.
    pub fn from_env() -> Overrides {
        let mut overrides = Overrides::default();
        for key in OVERRIDABLE {
            if let Ok(value) = std::env::var(format!("GOLEM_{key}")) {
                if !value.is_empty() {
                    overrides = overrides.with(key, &value);
                }
            }
        }
        overrides
    }

    /// The overrides a service request carried, by their `GOLEM_` names.
    /// A name outside [`OVERRIDABLE`] is refused: the protocol carries where
    /// a verb should point, never configuration at large.
    pub fn from_wire(pairs: &[(String, String)]) -> Result<Overrides, String> {
        let mut overrides = Overrides::default();
        for (name, value) in pairs {
            let key = name
                .strip_prefix("GOLEM_")
                .filter(|key| OVERRIDABLE.contains(key))
                .ok_or_else(|| format!("{name} is not an override a request may carry"))?;
            if !value.is_empty() {
                overrides = overrides.with(key, value);
            }
        }
        Ok(overrides)
    }

    /// The same overrides by their `GOLEM_` names, for the wire.
    pub fn wire(&self) -> Vec<(String, String)> {
        self.pairs.iter().map(|(key, value)| (format!("GOLEM_{key}"), value.clone())).collect()
    }

    /// These overrides with `key` set to `value`.
    pub fn with(mut self, key: &str, value: &str) -> Overrides {
        match self.pairs.iter_mut().find(|(k, _)| k == key) {
            Some((_, existing)) => *existing = value.to_string(),
            None => self.pairs.push((key.to_string(), value.to_string())),
        }
        self
    }

    /// These overrides with `top`'s laid over them: `top` wins per key.
    pub fn under(mut self, top: &Overrides) -> Overrides {
        for (key, value) in &top.pairs {
            self = self.with(key, value);
        }
        self
    }

    /// `key`: this run's override when it has one, else `golem.toml`'s.
    pub fn get(&self, home: &Path, key: &str) -> Option<String> {
        match self.pairs.iter().find(|(k, _)| k == key) {
            Some((_, value)) => Some(value.clone()),
            None => get(home, key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str, toml: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("golem-rs-config-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp");
        std::fs::write(dir.join("golem.toml"), toml).expect("write");
        dir
    }

    #[test]
    fn a_value_comes_from_golem_toml_and_nowhere_else() {
        let dir = home("get", "MODEL_ALIAS = \"moe-35b\"\n# ENGINE = \"x\"\nENGINE = \"llama.cpp\"\n");
        // A `.env` beside it is somebody else's file now.
        std::fs::write(dir.join(".env"), "LOCAL_AI_PORT=\"9999\"\n").expect("write");
        assert_eq!(get(&dir, "MODEL_ALIAS").as_deref(), Some("moe-35b"));
        assert_eq!(get(&dir, "ENGINE").as_deref(), Some("llama.cpp"));
        assert_eq!(get(&dir, "LOCAL_AI_PORT"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn setting_replaces_the_line_and_keeps_the_comments() {
        let dir = home("set", "# the model\nMODEL_ALIAS = \"old\"\n#MODELS_PRESET = \"x\"\n");
        set(&dir, "MODEL_ALIAS", "ornith-1.5-35b").expect("set");
        set(&dir, "MODELS_PRESET", "data/tuned/models.effective.ini").expect("set");
        assert_eq!(
            std::fs::read_to_string(dir.join("golem.toml")).expect("read"),
            "# the model\nMODEL_ALIAS = \"ornith-1.5-35b\"\n#MODELS_PRESET = \"x\"\n\
             MODELS_PRESET = \"data/tuned/models.effective.ini\"\n"
        );
        // No `.env` was created on the way.
        assert!(!dir.join(".env").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_override_wins_over_the_file_for_its_run_only() {
        let dir = home("override", "MODEL_ALIAS = \"coder\"\nLOCAL_AI_PORT = \"8000\"\n");
        let overrides = Overrides::default().with("MODEL_ALIAS", "tiny");
        assert_eq!(overrides.get(&dir, "MODEL_ALIAS").as_deref(), Some("tiny"));
        assert_eq!(overrides.get(&dir, "LOCAL_AI_PORT").as_deref(), Some("8000"));
        assert_eq!(Overrides::default().get(&dir, "MODEL_ALIAS").as_deref(), Some("coder"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_wire_form_round_trips_and_refuses_anything_else() {
        let overrides = Overrides::default().with("MODELS_PRESET", "data/x.ini").with("ENGINE", "llama.cpp");
        let wire = overrides.wire();
        assert_eq!(wire[0], ("GOLEM_MODELS_PRESET".to_string(), "data/x.ini".to_string()));
        assert_eq!(Overrides::from_wire(&wire), Ok(overrides));
        let stray = vec![("GOLEM_LOCAL_AI_API_KEY".to_string(), "x".to_string())];
        assert!(Overrides::from_wire(&stray).is_err(), "the key is not one a run may point elsewhere");
        let plain = vec![("ENGINE".to_string(), "x".to_string())];
        assert!(Overrides::from_wire(&plain).is_err(), "the wire spells them with GOLEM_");
    }

    #[test]
    fn a_request_lays_its_overrides_over_the_services_own() {
        // The container's bind address stays; the request's preset wins.
        let service = Overrides::default().with("LOCAL_AI_HOST", "0.0.0.0").with("MODELS_PRESET", "a.ini");
        let request = Overrides::default().with("MODELS_PRESET", "b.ini");
        let both = service.under(&request);
        let dir = home("under", "");
        assert_eq!(both.get(&dir, "LOCAL_AI_HOST").as_deref(), Some("0.0.0.0"));
        assert_eq!(both.get(&dir, "MODELS_PRESET").as_deref(), Some("b.ini"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
