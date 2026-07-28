//! antibising engine: PipeWire session management, device ranking, and
//! filter-chain control. No UI or Tauri dependencies — this crate is
//! consumed by the `antibisingd` daemon binary and its own test suite.

pub mod session;
pub mod model;

pub use model::{DeviceId, DeviceInfo, HealthStatus};
pub use session::{Session, SessionCommand, SessionEvent};
