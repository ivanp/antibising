//! The reconciler (U4): converges the actual PipeWire link graph toward the
//! routing policy's desired feed. Pure decision logic lives here
//! (`decide_action`); the effector (actual link create/destroy calls) is
//! wired into the session thread in `session.rs`, since link management
//! requires the live `Core`/`Registry`.
//!
//! KTD5's "adopt, don't blindly recreate" rule lives here: reconciliation
//! reads the *actual* current link on the capture node first. If it
//! already matches the desired device, nothing happens — this is what
//! makes a daemon restart non-disruptive (the `object.linger` link
//! survived the crash; reconciliation on restart must not tear it down and
//! rebuild it just because the process restarted).

use crate::model::DeviceId;
use crate::routing::DesiredFeed;

/// What the effector currently observes on the capture node: either no
/// link, or a link whose remote end resolves to a known device identity.
/// `Unknown` covers a link to a node the engine can't currently identify
/// (e.g. a stale/foreign link — WirePlumber interference, A1) and is always
/// treated as divergent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActualLink {
    None,
    ToDevice(DeviceId),
    Unknown,
}

/// The action the reconciler decides to take this cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Actual state already matches desired — do nothing. This is the
    /// common case and the one that makes restarts non-disruptive.
    NoOp,
    /// No link exists but one is desired — create it.
    Create(DeviceId),
    /// A link exists but none is desired — destroy it.
    Destroy,
    /// A link exists to the wrong device (or an unidentifiable one) —
    /// destroy then create against the desired device.
    Recreate(DeviceId),
}

/// Compute the reconciliation action for one cycle. Pure and total: never
/// panics, always returns exactly one action.
pub fn decide_action(desired: &DesiredFeed, actual: &ActualLink) -> ReconcileAction {
    match (desired, actual) {
        (DesiredFeed::None, ActualLink::None) => ReconcileAction::NoOp,
        (DesiredFeed::None, ActualLink::ToDevice(_) | ActualLink::Unknown) => {
            ReconcileAction::Destroy
        }
        (DesiredFeed::Device(d), ActualLink::None) => ReconcileAction::Create(d.clone()),
        (DesiredFeed::Device(d), ActualLink::ToDevice(actual_dev)) => {
            if d == actual_dev {
                ReconcileAction::NoOp
            } else {
                ReconcileAction::Recreate(d.clone())
            }
        }
        (DesiredFeed::Device(d), ActualLink::Unknown) => ReconcileAction::Recreate(d.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str) -> DeviceId {
        DeviceId(name.to_string())
    }

    #[test]
    fn nothing_desired_nothing_actual_is_noop() {
        assert_eq!(
            decide_action(&DesiredFeed::None, &ActualLink::None),
            ReconcileAction::NoOp
        );
    }

    #[test]
    fn nothing_desired_but_linked_destroys() {
        assert_eq!(
            decide_action(&DesiredFeed::None, &ActualLink::ToDevice(dev("a"))),
            ReconcileAction::Destroy
        );
    }

    #[test]
    fn nothing_desired_but_unknown_link_destroys() {
        assert_eq!(
            decide_action(&DesiredFeed::None, &ActualLink::Unknown),
            ReconcileAction::Destroy
        );
    }

    #[test]
    fn device_desired_no_link_creates() {
        assert_eq!(
            decide_action(&DesiredFeed::Device(dev("a")), &ActualLink::None),
            ReconcileAction::Create(dev("a"))
        );
    }

    #[test]
    fn device_desired_already_linked_to_it_is_noop() {
        // This is the KTD5 "adopt across restart" case: the desired device
        // matches what's actually linked (e.g. a lingering link survived a
        // daemon crash) -- must not destroy+recreate.
        assert_eq!(
            decide_action(&DesiredFeed::Device(dev("a")), &ActualLink::ToDevice(dev("a"))),
            ReconcileAction::NoOp
        );
    }

    #[test]
    fn device_desired_but_linked_to_different_device_recreates() {
        assert_eq!(
            decide_action(&DesiredFeed::Device(dev("a")), &ActualLink::ToDevice(dev("b"))),
            ReconcileAction::Recreate(dev("a"))
        );
    }

    #[test]
    fn device_desired_but_unknown_link_recreates() {
        // Covers WirePlumber interference (A1): a rogue link to an
        // unidentifiable node must be corrected, not adopted.
        assert_eq!(
            decide_action(&DesiredFeed::Device(dev("a")), &ActualLink::Unknown),
            ReconcileAction::Recreate(dev("a"))
        );
    }
}
