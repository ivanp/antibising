//! antibising engine: PipeWire session management, device ranking, and
//! filter-chain control. No UI or Tauri dependencies — this crate is
//! consumed by the `antibisingd` daemon binary and its own test suite.

pub mod config;
pub mod fragment;
pub mod install;
pub mod meter;
pub mod model;
pub mod monitor;
pub mod params;
pub mod reconcile;
pub mod routing;
pub mod session;

pub use config::{Config, ConfigError};
pub use fragment::{
    guard_denoise_config, render as render_fragment, write_atomic, FragmentConfig, SOURCE_NAME,
    DEFAULT_LADSPA_PLUGIN_PATH,
};
pub use install::{classify_startup, install, is_unit_active, uninstall, InstallPaths, StartupAction};
pub use meter::{compute_frame, MeterChannel, MeterFrame};
pub use model::{compute_health, DeviceId, DeviceInfo, HealthStatus};
pub use monitor::{assess_speaker_risk, SpeakerRisk};
pub use params::{build_props_pod, RnnoiseParam};
pub use session::{Session, SessionCommand, SessionEvent};
