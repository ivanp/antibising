//! Pure data model: device identity, filtering rules, and health status.
//! No PipeWire I/O in this module — keeps the ranking/identity logic unit
//! testable without a live daemon (per the plan's Verification Contract).

use std::fmt;

/// A stable, bus-native device identity key — never a `node.name` or any
/// volatile node property. See the plan's "Device identity" table:
/// USB keys on `device.serial`, Bluetooth on the MAC address, internal
/// (PCI/HDA) on the PCI address. All three are exposed on the **device**
/// object, not the node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct DeviceId(pub String);

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A physical microphone currently visible on the PipeWire graph, resolved
/// to its `Audio/Source` node.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeviceInfo {
    /// Stable device-object identity (see [`DeviceId`]).
    pub id: DeviceId,
    /// Current `Audio/Source` node id. Volatile across restarts/profile
    /// switches — never persisted, only used for the current link.
    pub node_id: u32,
    /// Human-readable name for the panel (device.description / node.description).
    pub description: String,
}

/// Whether a node counts as a candidate physical microphone.
///
/// Excludes:
/// - our own permanent source (matched by `node.name`)
/// - any node whose `media.class` ends in `/Internal` — Bluetooth profile
///   switches create/destroy these on every transition (Q2); treating them
///   as devices produces phantom arrivals/departures.
pub fn is_candidate_source(media_class: &str, node_name: &str, our_source_name: &str) -> bool {
    if node_name == our_source_name {
        return false;
    }
    if media_class.ends_with("/Internal") {
        return false;
    }
    media_class == "Audio/Source"
}

/// Health verdict the engine reports for the permanent source. R5's honesty
/// requirement starts here: the UI renders this verbatim, never a stronger
/// claim than the engine has verified.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HealthStatus {
    /// Linked to a live device and confirmed flowing.
    Linked { device: DeviceId, description: String },
    /// The source exists and is `RUNNING`, but no device currently feeds it.
    /// A legal, non-error state per R4.
    SilentNoDevice,
    /// Something is wrong and self-repair failed. `reason` is precise enough
    /// to show the user directly (R5, R7).
    Broken { reason: String },
    /// Lost the PipeWire connection; reconnecting. Distinct from `Broken` —
    /// this is an expected transient during a PipeWire restart/crash (U1).
    Reconnecting,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_internal_media_class() {
        assert!(!is_candidate_source(
            "Audio/Source/Internal",
            "bluez_input.AA_BB.0",
            "antibising_mic"
        ));
    }

    #[test]
    fn excludes_our_own_source_by_name() {
        assert!(!is_candidate_source(
            "Audio/Source",
            "antibising_mic",
            "antibising_mic"
        ));
    }

    #[test]
    fn excludes_non_source_media_classes() {
        assert!(!is_candidate_source(
            "Audio/Sink",
            "some_sink",
            "antibising_mic"
        ));
        assert!(!is_candidate_source(
            "Stream/Input/Audio/Internal",
            "some_stream",
            "antibising_mic"
        ));
    }

    #[test]
    fn accepts_plain_audio_source() {
        assert!(is_candidate_source(
            "Audio/Source",
            "alsa_input.usb-Razer",
            "antibising_mic"
        ));
        assert!(is_candidate_source(
            "Audio/Source",
            "bluez_input.AA:BB:CC:DD:EE:FF",
            "antibising_mic"
        ));
    }
}
