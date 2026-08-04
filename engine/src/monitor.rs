//! Pure monitoring logic (U6/R9): the speaker-vs-headset feedback
//! warning. No PipeWire I/O — the actual link creation (source output
//! ports -> default sink input ports) lives in `session.rs`, which is
//! where the "verified mechanism, no module" finding this session
//! measured applies; this module only decides whether to warn.
//!
//! **Heuristic, not authoritative** — the plan is explicit: "warn, don't
//! refuse." `device.form_factor` lives on the sink's owning **Device**
//! global (verified this session: present on Logitech G435 headset and
//! Fantech webcam cards, absent on this machine's plain HDA sinks), not
//! reliably on the `Audio/Sink` node itself, so a caller that hasn't
//! resolved the owning device still gets a usable answer from name-based
//! fallback heuristics on the sink's own `node.name`/`node.description`.

/// Whether a monitor destination is likely to cause audio feedback
/// (speaker) vs. safe (headphones/headset). `Unknown` is honest when
/// neither the form-factor nor any name heuristic matches anything — R9
/// says warn, don't refuse, and refusing to guess is not the same as
/// asserting "safe."
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SpeakerRisk {
    /// A form-factor or name signal points at headphones/headset — no
    /// warning needed.
    LikelyHeadphones,
    /// A form-factor or name signal points at an open speaker — R9's
    /// warning must surface.
    LikelySpeaker,
    /// No usable signal either way.
    Unknown,
}

/// Decide [`SpeakerRisk`] from whatever properties are available.
/// `device_form_factor` is the owning Device global's `device.form_factor`
/// if the caller resolved it (may be absent — many sinks don't carry
/// one, verified this session); `name` is the sink's own
/// `node.description` or `node.name`, always available.
///
/// Form-factor, when present, is authoritative over the name heuristic —
/// it's the more deliberate signal. Name matching is case-insensitive and
/// substring-based (real names observed this session: "Meteor Lake-P HD
/// Audio Controller Headphones", "G435 Wireless Gaming Headset").
pub fn assess_speaker_risk(device_form_factor: Option<&str>, name: &str) -> SpeakerRisk {
    if let Some(form_factor) = device_form_factor {
        match form_factor {
            "headphone" | "headset" | "hands-free" | "portable" => {
                return SpeakerRisk::LikelyHeadphones
            }
            "speaker" | "car" | "hifi" | "tv" => return SpeakerRisk::LikelySpeaker,
            _ => {} // "internal", "microphone", "webcam", unknown values -> fall through
        }
    }

    let lower = name.to_lowercase();
    if lower.contains("headphone") || lower.contains("headset") || lower.contains("earbud") {
        return SpeakerRisk::LikelyHeadphones;
    }
    if lower.contains("speaker") || lower.contains("hdmi") || lower.contains("built-in") {
        return SpeakerRisk::LikelySpeaker;
    }

    SpeakerRisk::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_factor_headset_is_authoritative_over_misleading_name() {
        assert_eq!(
            assess_speaker_risk(Some("headset"), "Generic Audio Device"),
            SpeakerRisk::LikelyHeadphones
        );
    }

    #[test]
    fn form_factor_speaker_is_authoritative() {
        assert_eq!(
            assess_speaker_risk(Some("speaker"), "Generic Audio Device"),
            SpeakerRisk::LikelySpeaker
        );
    }

    #[test]
    fn no_form_factor_falls_back_to_name_headphones() {
        assert_eq!(
            assess_speaker_risk(None, "Meteor Lake-P HD Audio Controller Headphones"),
            SpeakerRisk::LikelyHeadphones
        );
    }

    #[test]
    fn no_form_factor_falls_back_to_name_speaker() {
        assert_eq!(
            assess_speaker_risk(None, "Built-in Speaker"),
            SpeakerRisk::LikelySpeaker
        );
    }

    #[test]
    fn hdmi_output_is_treated_as_speaker_risk() {
        // HDMI/DisplayPort outputs usually drive open TV/monitor speakers
        // -- verified this session's own sink list (HDMI1/2/3 sinks on
        // this machine).
        assert_eq!(
            assess_speaker_risk(None, "Meteor Lake-P HD Audio Controller HDMI / DisplayPort 1 Output"),
            SpeakerRisk::LikelySpeaker
        );
    }

    #[test]
    fn no_signal_at_all_is_honestly_unknown_not_assumed_safe() {
        assert_eq!(assess_speaker_risk(None, "USB Audio Device"), SpeakerRisk::Unknown);
    }

    #[test]
    fn unrecognized_form_factor_falls_through_to_name_heuristic() {
        // "webcam" form-factor (a real value observed this session) is
        // neither headphone nor speaker -- must fall through to the name
        // check rather than defaulting either way.
        assert_eq!(
            assess_speaker_risk(Some("webcam"), "Fantech Webcam Speaker"),
            SpeakerRisk::LikelySpeaker
        );
    }
}
