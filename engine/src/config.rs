//! Persisted daemon configuration (KTD6/Q5): device ranking, pin state,
//! denoise settings. This TOML file is the source of truth; both the
//! filter-chain fragment (`fragment.rs`) and runtime `set-param` calls
//! (`params.rs`) are *derived* from it, never the reverse. Verified this
//! session: a live `set-param` value does not survive a `filter-chain.service`
//! restart — only the fragment does, and the fragment is only regenerated
//! from this config. So every user-facing control write goes through here
//! first, then fans out to the two runtime mechanisms.
//!
//! Round-trips are atomic (`fragment.rs::write_atomic`, reused here) for
//! the same reason the fragment write is: a crash mid-write must never
//! leave a config file `toml::from_str` can't parse on the next daemon
//! start.

use crate::fragment::FragmentConfig;
use std::path::Path;

/// The full persisted config. Field names are the TOML keys — kept flat
/// and stable since this file is the durable half of every control (R8's
/// two-layer persistence), not an internal implementation detail free to
/// churn.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// Ranked device preference list (R3), most-preferred first. Keys on
    /// the device object's stable identity string (never a node name).
    #[serde(default)]
    pub preference_order: Vec<String>,
    /// The currently pinned device, if any (R3). `None` = unpinned,
    /// ranking active.
    #[serde(default)]
    pub pin: Option<String>,
    /// Whether RNNoise is in the graph at all.
    #[serde(default = "default_denoise_enabled")]
    pub denoise_enabled: bool,
    /// RNNoise `VAD Threshold (%)` — R8's primary strength control.
    #[serde(default = "default_vad_threshold")]
    pub vad_threshold: f64,
    /// RNNoise `Dry Mix` — the denoise on/off toggle (R6/U5). `0.0` =
    /// suppression on; `1.0` = clean passthrough.
    #[serde(default = "default_dry_mix")]
    pub dry_mix: f64,
}

fn default_denoise_enabled() -> bool {
    FragmentConfig::default().denoise_enabled
}
fn default_vad_threshold() -> f64 {
    FragmentConfig::default().vad_threshold
}
fn default_dry_mix() -> f64 {
    FragmentConfig::default().dry_mix
}

impl Default for Config {
    fn default() -> Self {
        let fragment_defaults = FragmentConfig::default();
        Self {
            preference_order: Vec::new(),
            pin: None,
            denoise_enabled: fragment_defaults.denoise_enabled,
            vad_threshold: fragment_defaults.vad_threshold,
            dry_mix: fragment_defaults.dry_mix,
        }
    }
}

impl Config {
    /// The denoise-relevant subset, as the [`FragmentConfig`] the fragment
    /// renderer needs. Kept as an explicit narrow projection (not a shared
    /// struct) so `Config` can grow fields the fragment never needs to
    /// know about without coupling the two.
    pub fn to_fragment_config(&self) -> FragmentConfig {
        FragmentConfig {
            denoise_enabled: self.denoise_enabled,
            vad_threshold: self.vad_threshold,
            dry_mix: self.dry_mix,
        }
    }

    /// The daemon's own config file, under the same `~/.config` root
    /// [`crate::install::InstallPaths`] uses — but kept separate from
    /// that struct, since `install`/`uninstall` never touch this file
    /// (it's daemon-owned runtime state, not part of the bootstrap the
    /// Product Contract's Dependencies section lists).
    pub fn default_path() -> std::path::PathBuf {
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::path::PathBuf::from(std::env::var("HOME").expect("HOME must be set"))
                    .join(".config")
            });
        base.join("antibising/config.toml")
    }

    /// Load from `path`. Missing file is `Ok(default)` — a first daemon
    /// run has no config yet, and that must not be an error (R7: repair,
    /// don't refuse). A present-but-corrupt file surfaces its parse error
    /// so the caller can decide what "corrupt" means for their context
    /// (the daemon's own Q4-style startup reconciliation, not this
    /// module's job).
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(content) => toml::from_str(&content).map_err(ConfigError::Parse),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io(e)),
        }
    }

    /// Serialize and write atomically (same `rename()`-based mechanism as
    /// the fragment — a crash mid-write must never leave a truncated file
    /// the next `load()` can't parse).
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let content = toml::to_string_pretty(self).map_err(ConfigError::Serialize)?;
        crate::fragment::write_atomic(path, &content).map_err(ConfigError::Io)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("failed to serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_loads_as_default() {
        let dir = std::env::temp_dir().join(format!("antibising-config-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nonexistent.toml");
        let config = Config::load(&path).expect("missing file must load as default, not error");
        assert_eq!(config, Config::default());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_then_load_round_trips_exactly() {
        let dir = std::env::temp_dir().join(format!("antibising-config-roundtrip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let config = Config {
            preference_order: vec!["dev-a".to_string(), "dev-b".to_string()],
            pin: Some("dev-a".to_string()),
            denoise_enabled: true,
            vad_threshold: 72.5,
            dry_mix: 0.25,
        };
        config.save(&path).expect("save");
        let read_back = Config::load(&path).expect("load");
        assert_eq!(config, read_back);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_file_surfaces_parse_error() {
        let dir = std::env::temp_dir().join(format!("antibising-config-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, b"this is not valid toml {{{").unwrap();

        let result = Config::load(&path);
        assert!(matches!(result, Err(ConfigError::Parse(_))));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn partial_toml_fills_missing_fields_from_defaults() {
        // A config written by an older schema version, or hand-edited to
        // drop a field, must not fail to load -- every field has a
        // default (#[serde(default)]).
        let dir = std::env::temp_dir().join(format!("antibising-config-partial-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, b"preference_order = [\"dev-a\"]\n").unwrap();

        let config = Config::load(&path).expect("partial config must load with defaults filled in");
        assert_eq!(config.preference_order, vec!["dev-a".to_string()]);
        assert_eq!(config.pin, None);
        assert_eq!(config.denoise_enabled, Config::default().denoise_enabled);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn to_fragment_config_projects_denoise_fields_only() {
        let config = Config {
            preference_order: vec!["dev-a".to_string()],
            pin: Some("dev-a".to_string()),
            denoise_enabled: true,
            vad_threshold: 60.0,
            dry_mix: 0.5,
        };
        let fragment_config = config.to_fragment_config();
        assert_eq!(fragment_config.denoise_enabled, true);
        assert_eq!(fragment_config.vad_threshold, 60.0);
        assert_eq!(fragment_config.dry_mix, 0.5);
    }
}
