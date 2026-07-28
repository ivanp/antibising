//! Pure ranked-routing logic (U4 / R3). No PipeWire I/O here — kept
//! separate from the effector (link creation/destruction in `reconcile.rs`)
//! so ranking decisions are unit-testable without a live daemon.

use crate::model::DeviceId;
use std::collections::HashSet;

/// What the routing policy currently wants fed into the permanent source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesiredFeed {
    /// Feed from this specific device.
    Device(DeviceId),
    /// No device present; the source should have no capture-side link.
    /// Legal per R4 — not an error.
    None,
}

/// Compute which device should feed the source right now.
///
/// Rules (R3):
/// - A pinned device wins outright *if it is currently present* — pin
///   suppresses ranking entirely while active.
/// - If the pinned device is not present, the pin has already been
///   released by the caller (see `release_pin_if_absent`); this function
///   never guesses at that policy itself, it only takes `pin` at face
///   value for the present check.
/// - Otherwise, the highest-ranked present device in `preference_order`
///   wins. Devices not present are skipped; devices present but not in the
///   preference list are never chosen (only recognized devices in the
///   user's list are ranked).
/// - No candidate present -> `DesiredFeed::None`.
pub fn compute_desired_feed(
    present: &HashSet<DeviceId>,
    preference_order: &[DeviceId],
    pin: Option<&DeviceId>,
) -> DesiredFeed {
    if let Some(pinned) = pin {
        if present.contains(pinned) {
            return DesiredFeed::Device(pinned.clone());
        }
        // Pinned device absent: per R3, disappearance releases the pin and
        // ranking resumes. This function is pure and stateless, so it
        // doesn't mutate any stored pin — the caller (reconcile.rs) is
        // responsible for clearing its own pin state when it observes this
        // fallthrough, via `should_release_pin`.
    }

    for candidate in preference_order {
        if present.contains(candidate) {
            return DesiredFeed::Device(candidate.clone());
        }
    }

    DesiredFeed::None
}

/// Whether a stored pin should be released because its device is no longer
/// present. Called by the reconciler on every device-set change; kept as a
/// separate pure predicate so the "pin released on disappearance" rule is
/// independently testable from the feed computation above.
pub fn should_release_pin(pin: Option<&DeviceId>, present: &HashSet<DeviceId>) -> bool {
    match pin {
        Some(p) => !present.contains(p),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> HashSet<DeviceId> {
        names.iter().map(|n| DeviceId(n.to_string())).collect()
    }

    fn order(names: &[&str]) -> Vec<DeviceId> {
        names.iter().map(|n| DeviceId(n.to_string())).collect()
    }

    #[test]
    fn highest_ranked_present_device_wins() {
        let present = ids(&["b", "c"]);
        let pref = order(&["a", "b", "c"]); // a not present
        let feed = compute_desired_feed(&present, &pref, None);
        assert_eq!(feed, DesiredFeed::Device(DeviceId("b".into())));
    }

    #[test]
    fn pin_beats_rank() {
        let present = ids(&["a", "b"]);
        let pref = order(&["a", "b"]); // a would win by rank
        let pin = DeviceId("b".into());
        let feed = compute_desired_feed(&present, &pref, Some(&pin));
        assert_eq!(feed, DesiredFeed::Device(DeviceId("b".into())));
    }

    #[test]
    fn pin_released_on_departure_falls_back_to_ranking() {
        let present = ids(&["a"]); // pinned device "b" no longer present
        let pref = order(&["a", "b"]);
        let pin = DeviceId("b".into());
        // compute_desired_feed with a now-absent pin falls through to
        // ranking (it does not error or return None just because the pin
        // is stale).
        let feed = compute_desired_feed(&present, &pref, Some(&pin));
        assert_eq!(feed, DesiredFeed::Device(DeviceId("a".into())));
    }

    #[test]
    fn should_release_pin_true_when_pinned_device_absent() {
        let present = ids(&["a"]);
        let pin = DeviceId("b".into());
        assert!(should_release_pin(Some(&pin), &present));
    }

    #[test]
    fn should_release_pin_false_when_pinned_device_present() {
        let present = ids(&["a", "b"]);
        let pin = DeviceId("b".into());
        assert!(!should_release_pin(Some(&pin), &present));
    }

    #[test]
    fn should_release_pin_false_when_no_pin() {
        let present = ids(&["a"]);
        assert!(!should_release_pin(None, &present));
    }

    #[test]
    fn empty_present_set_yields_no_feed() {
        let present: HashSet<DeviceId> = HashSet::new();
        let pref = order(&["a", "b"]);
        assert_eq!(compute_desired_feed(&present, &pref, None), DesiredFeed::None);
    }

    #[test]
    fn device_present_but_not_in_preference_list_is_never_chosen() {
        let present = ids(&["z"]); // present but unranked
        let pref = order(&["a", "b"]);
        assert_eq!(compute_desired_feed(&present, &pref, None), DesiredFeed::None);
    }

    #[test]
    fn lower_ranked_device_ignored_when_higher_ranked_also_present() {
        let present = ids(&["a", "b", "c"]);
        let pref = order(&["c", "a", "b"]); // c ranks highest here
        assert_eq!(
            compute_desired_feed(&present, &pref, None),
            DesiredFeed::Device(DeviceId("c".into()))
        );
    }
}
