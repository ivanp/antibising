//! Live `set-param` control of the RNNoise node inside the filter-chain
//! graph (U5/R8). Two measured traps this module exists to make
//! unrepresentable at the call site:
//!
//! 1. **The key must be prefixed with the filter-graph node name**
//!    (`"rnnoise"`, matching `fragment.rs`'s node name) — the plugin
//!    filename or LADSPA label is accepted but silently does nothing.
//! 2. **The call must target the capture node** (`node.name` of the
//!    input half, `"<source>_capture"`), not the `Audio/Source` node —
//!    targeting the source half is likewise accepted and silently
//!    ignored.
//!
//! [`RnnoiseParam::key`] bakes the required prefix in; callers can never
//! construct an unprefixed key. Which node id to target is the caller's
//! responsibility (`session.rs` tracks `capture_node_id`, never the
//! source node id, for exactly this reason).
//!
//! ## `Dry Mix` is the denoise toggle (R6)
//!
//! RNNoise's own `Dry Mix` control (0.0-1.0) is a real audio-domain
//! parameter on the `noise_suppressor_mono` LADSPA node, not a separate
//! bypass/copy branch. Verified live against a scratch fragment and a
//! deterministic tone+noise fixture: `Dry Mix=1.0` passed the fixture
//! through unmodified (RMS 0.257, matching direct bypass); `Dry Mix=0.0`
//! drove RNNoise's own VAD-gated suppression (RMS 0.0 on the synthetic
//! non-speech fixture, as expected — RNNoise gates on speech likelihood,
//! not generic energy). One existing node, one control, no restart.

use pipewire::spa;

/// A live-adjustable RNNoise control, keyed by the filter-graph node name
/// `fragment.rs` assigns (`"rnnoise"`). Each variant knows its own LADSPA
/// port name and value range; there is no way to construct a param key
/// without going through here, so the node-name-prefix trap from
/// measurement cannot be reproduced by a caller.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RnnoiseParam {
    /// `VAD Threshold (%)`, range 0.0-99.0 (LADSPA descriptor bound).
    VadThreshold,
    /// `Dry Mix`, range 0.0-1.0. `0.0` = full suppression (denoise on);
    /// `1.0` = clean passthrough (denoise off), RNNoise node still in
    /// the graph.
    DryMix,
    /// `VAD Grace Period (ms)`, range 0.0-1000.0.
    VadGracePeriod,
    /// `Retroactive VAD Grace (ms)`, range 0.0-200.0.
    RetroactiveVadGrace,
}

impl RnnoiseParam {
    /// The LADSPA port name, exactly as `noise_suppressor_mono`'s
    /// descriptor names it (confirmed via a direct descriptor probe
    /// against the real `ladspa.h` struct layout, not `strings`
    /// guesswork).
    fn ladspa_port_name(self) -> &'static str {
        match self {
            RnnoiseParam::VadThreshold => "VAD Threshold (%)",
            RnnoiseParam::DryMix => "Dry Mix",
            RnnoiseParam::VadGracePeriod => "VAD Grace Period (ms)",
            RnnoiseParam::RetroactiveVadGrace => "Retroactive VAD Grace (ms)",
        }
    }

    /// The full `set-param` key: the filter-graph node name (`"rnnoise"`,
    /// matching `fragment.rs`) prefixed onto the LADSPA port name. This is
    /// the ONLY place this prefix is assembled — never inline it at a
    /// call site, or the wrong-prefix trap becomes reproducible again.
    pub fn key(self) -> String {
        format!("rnnoise:{}", self.ladspa_port_name())
    }

    /// The control's valid range, taken from the LADSPA descriptor's
    /// `PortRangeHints` (confirmed via the same descriptor probe).
    pub fn range(self) -> (f32, f32) {
        match self {
            RnnoiseParam::VadThreshold => (0.0, 99.0),
            RnnoiseParam::DryMix => (0.0, 1.0),
            RnnoiseParam::VadGracePeriod => (0.0, 1000.0),
            RnnoiseParam::RetroactiveVadGrace => (0.0, 200.0),
        }
    }

    /// Clamp `value` into this control's valid range. `set-param` on an
    /// out-of-range value is accepted by PipeWire without complaint and
    /// silently clamped or ignored by the plugin depending on the LADSPA
    /// host's bounds-checking — clamping here keeps callers honest rather
    /// than relying on that undocumented downstream behavior.
    pub fn clamp(self, value: f32) -> f32 {
        let (lo, hi) = self.range();
        value.clamp(lo, hi)
    }
}

/// Build the raw pod bytes for a single-param `Props` `set-param` call:
/// `SPA_TYPE_OBJECT_Props` containing one `params` property, whose value
/// is the `Struct(String key, Float value)` pair filter-chain's
/// custom-control mechanism expects (the same shape `pw-cli set-param`
/// sends, confirmed by reading back an equivalent call's `enum-params`
/// output live).
///
/// Pure and PipeWire-connection-free — the byte layout is exercised by
/// unit tests without a live session; only the actual `Node::set_param`
/// call (in `session.rs`, where the bound `Node` proxy lives) needs one.
pub fn build_props_pod(param: RnnoiseParam, value: f32) -> Vec<u8> {
    let clamped = param.clamp(value);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamProps.as_raw(),
        id: spa::param::ParamType::Props.as_raw(),
        properties: vec![spa::pod::Property::new(
            spa::sys::SPA_PROP_params,
            spa::pod::Value::Struct(vec![
                spa::pod::Value::String(param.key()),
                spa::pod::Value::Float(clamped),
            ]),
        )],
    };
    let value = spa::pod::Value::Object(obj);
    let (cursor, _size) =
        spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
            .expect("Props/params pod serialization cannot fail for well-formed input");
    cursor.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_always_carries_the_filter_graph_node_prefix() {
        // The wrong-prefix trap from measurement: a caller must never be
        // able to construct a bare "Dry Mix" or "VAD Threshold (%)" key —
        // PipeWire accepts it and silently does nothing.
        assert_eq!(RnnoiseParam::DryMix.key(), "rnnoise:Dry Mix");
        assert_eq!(RnnoiseParam::VadThreshold.key(), "rnnoise:VAD Threshold (%)");
        assert_eq!(
            RnnoiseParam::VadGracePeriod.key(),
            "rnnoise:VAD Grace Period (ms)"
        );
        assert_eq!(
            RnnoiseParam::RetroactiveVadGrace.key(),
            "rnnoise:Retroactive VAD Grace (ms)"
        );
    }

    #[test]
    fn dry_mix_clamps_to_zero_one() {
        assert_eq!(RnnoiseParam::DryMix.clamp(-0.5), 0.0);
        assert_eq!(RnnoiseParam::DryMix.clamp(1.5), 1.0);
        assert_eq!(RnnoiseParam::DryMix.clamp(0.5), 0.5);
    }

    #[test]
    fn vad_threshold_clamps_to_descriptor_range() {
        assert_eq!(RnnoiseParam::VadThreshold.clamp(-10.0), 0.0);
        assert_eq!(RnnoiseParam::VadThreshold.clamp(150.0), 99.0);
        assert_eq!(RnnoiseParam::VadThreshold.clamp(72.5), 72.5);
    }

    #[test]
    fn pod_serializes_to_nonempty_valid_bytes() {
        let bytes = build_props_pod(RnnoiseParam::DryMix, 1.0);
        assert!(!bytes.is_empty());
        let pod = spa::pod::Pod::from_bytes(&bytes).expect("serialized pod must parse back");
        assert!(pod.size() > 0);
    }

    #[test]
    fn pod_round_trips_through_deserialization_with_prefixed_key_and_clamped_value() {
        // Not just "it serializes" — assert the actual key string and
        // value the plugin will read land in the bytes unchanged. This is
        // the difference between "compiles" and "sends the pod pw-cli
        // sent live", per this session's finding that acceptance without
        // readback is not proof of effect.
        let bytes = build_props_pod(RnnoiseParam::DryMix, 2.0); // out of range, must clamp
        let pod = spa::pod::Pod::from_bytes(&bytes).unwrap();
        let (_, value) = spa::pod::deserialize::PodDeserializer::deserialize_any_from(pod.as_bytes())
            .expect("deserialize the pod we just built");
        let spa::pod::Value::Object(obj) = value else {
            panic!("expected an Object pod");
        };
        assert_eq!(obj.id, spa::param::ParamType::Props.as_raw());
        assert_eq!(obj.properties.len(), 1);
        let prop = &obj.properties[0];
        assert_eq!(prop.key, spa::sys::SPA_PROP_params);
        let spa::pod::Value::Struct(fields) = &prop.value else {
            panic!("expected a Struct value for the params prop");
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0], spa::pod::Value::String("rnnoise:Dry Mix".to_string()));
        assert_eq!(fields[1], spa::pod::Value::Float(1.0), "value must be clamped to the 0.0-1.0 range before serialization");
    }
}
