//! antibising engine: PipeWire session management, device ranking, and
//! filter-chain control. No UI or Tauri dependencies — this crate is
//! consumed by the `antibisingd` daemon binary and its own test suite.

pub mod fragment;
pub mod install;
pub mod model;
pub mod reconcile;
pub mod routing;
pub mod session;

pub use fragment::{render as render_fragment, write_atomic, FragmentConfig, SOURCE_NAME};
pub use install::{classify_startup, install, is_unit_active, uninstall, InstallPaths, StartupAction};
pub use model::{DeviceId, DeviceInfo, HealthStatus};
pub use session::{Session, SessionCommand, SessionEvent};
