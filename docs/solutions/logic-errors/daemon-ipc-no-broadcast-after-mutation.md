---
title: "Daemon never broadcast a confirmation event after a config mutation, leaving the Denoise toggle stuck"
date: 2026-08-04
category: logic-errors
module: daemon/ipc + engine/session
problem_type: logic_error
component: tooling
severity: high
symptoms:
  - Denoise button cannot be switched on or off in the app panel
  - Hear yourself monitoring plays back with one channel silent (mono source not fanned out to both sink channels)
root_cause: logic_error
resolution_type: code_fix
tags:
  - daemon-ipc
  - event-broadcast
  - missing-confirmation
  - pipewire
  - session-monitor
  - mono-fanout
  - tauri
---

# Daemon never broadcast a confirmation event after a config mutation, leaving the Denoise toggle stuck

## Problem

Two user-visible bugs surfaced together in the same debugging session, both on the running antibising desktop app against the live `antibisingd` daemon: (1) the **Denoise** button "is not working, i cant switch it on or off" — clicks never visibly flipped the toggle; and (2) **Hear yourself** monitoring sounded wrong ("my voice is too fast, i guess need to add like 100ms?") — which live inspection pinned down as one speaker channel being completely silent, not a timing artifact. Both were missing-code-path defects rather than failures of the state-mutation logic itself: the daemon applied every requested change to its config and to the live PipeWire graph correctly, but in both cases a client-facing consequence was silently dropped.

## Symptoms

- **Denoise toggle**: clicking the panel button produced no visible change in the button's label. The daemon side *was* changing state (persisted `config.toml`'s `dry_mix` and the live `rnnoise:Dry Mix` LADSPA param both flipped — verified over raw IPC), but no confirmation event ever reached the panel, so its UI never re-rendered.
- **Hear yourself**: only one channel of the default sink was linked from the permanent mic source. The monitor session reported success (`MonitorStarted`), but `pw-dump` of the live graph showed the source's single output port linked only to `playback_FL` of the default sink — `playback_FR` had no incoming link, i.e. one ear silent.
- A third reported symptom ("voice sped up" / a guessed ~100ms latency fix) is **explicitly not fixed and not claimed as fixed here.** One silent channel is not a pitch/tempo artifact, so it does not mechanically match either bug below. An unmeasured candidate mechanism (two separate PipeWire clock domains — the mic's own clock vs. the sink's self-driven clock — plus `resample.disable=true` set on the source node) was flagged to the user as an unconfirmed hypothesis requiring an actual recording/comparison, not fabricated as a fix.

## What Didn't Work

Two real investigative dead ends, both worth preserving:

**1. An over-broad first version of the broadcast fix.** An earlier version of the fix broadcast a `Snapshot` unconditionally after *every* successful `apply_request` call, config-mutating or not. That broke four existing `daemon/tests/ipc.rs` tests — `start_monitor_forwards_command_and_routes_reply_to_requester_only`, `start_monitor_failure_reaches_requester_with_reason`, `start_meter_forwards_command_and_drains_frames_to_client`, and `command_from_one_client_broadcasts_delta_to_both` — because session-scoped requests (`StartMonitor`/`StartMeter`) got a redundant `Snapshot` broadcast to every *other* connected client on top of their own dedicated per-session reply path. All four tests still exist in the current file. The fix was narrowed to only broadcast when the mutation actually touched `config` (see Solution below), which resolved all four failures.

**2. A false-negative live-test methodology trap.** The first attempt to verify the monitor-link fix used a raw Python script that connected over the Unix socket, sent `StartMonitor`, read the reply, and then let the script exit — closing the socket immediately. `pw-dump` taken right after showed **no** link at all, a false negative: the daemon's connection-cleanup path auto-releases the connection's sessions on disconnect — `handle_client` sends `SessionCommand::StopMonitor(session_id)` when the read loop exits (`daemon/src/ipc.rs:636`), which tears the monitor links down (the link proxies are created without `object.linger`, so dropping them destroys the links server-side — see the comment at `engine/src/session.rs` around the link-creation site). The links were real; the test just observed them after they were already gone. The fix for the test methodology itself was to hold the IPC connection open (e.g. `time.sleep(N)` before closing) while inspecting the live PipeWire graph from a separate process.

## Solution

Two independent fixes, both landed in commit `32e57cd` (branch `development`):

### Fix 1: broadcast a fresh Snapshot on config mutation (`daemon/src/ipc.rs`)

`apply_request`'s return type changed from `Result<(), String>` to `Result<bool, String>`. Before (parent commit `ba00b95`) — note `start_monitor` itself was introduced in the same commit as the fix (`32e57cd`), so this "before" state describes the confirmation-broadcast defect only, not a pre-existing `start_monitor`:

```rust
pub fn apply_request(
    request: Request,
    config: &mut Config,
    session_cmd_tx: &std::sync::mpsc::Sender<SessionCommand>,
) -> Result<(), String> {
    match request {
        Request::RequestSnapshot => { /* ... */ Ok(()) }
        Request::SetPin { device } => { /* ... */ Ok(()) }
        // ... SetPreferenceOrder, SetThreshold, ToggleDenoise all Ok(())
    }
}
```

and the call site in `handle_client` only persisted config, never notified anyone:

```rust
match apply_request(request, &mut state.config, &session_cmd_tx) {
    Ok(()) => {
        let content = toml::to_string_pretty(&state.config)
            .expect("Config must always serialize to TOML");
        let _ = engine::write_atomic(&state.config_path, &content);
    }
    Err(message) => { /* ... */ }
}
```

After (current tree, `daemon/src/ipc.rs:415-486`): each branch returns `true` iff it mutated `config` — `SetPin` (`:427`), `SetPreferenceOrder` (`:433`), `SetThreshold` (`:439`), `ToggleDenoise` (`:448`) all return `Ok(true)`; the session-scoped requests `RequestSnapshot` (`:423`), `StartMonitor` (`:463`), `StopMonitor` (`:467`), `StartMeter` (`:471`), `StopMeter` (`:482`) return `Ok(false)`. The call site (`daemon/src/ipc.rs:573-609`):

```rust
match apply_request(request, &mut state.config, &session_cmd_tx, session_id, &meter_slot) {
    Ok(config_changed) => {
        if config_changed {
            let content = toml::to_string_pretty(&state.config)
                .expect("Config must always serialize to TOML");
            let _ = engine::write_atomic(&state.config_path, &content);
            let snapshot = state.snapshot_event();
            drop(state);
            broadcaster.broadcast(&snapshot);
        }
    }
    Err(message) => { /* unchanged error path */ }
}
```

A full `Snapshot` event — not a narrower delta — is broadcast to **all** connected clients, including the sender, only when the bool is true. This matches the file's own existing doc comment that `Snapshot` is "enough for a client to render its whole UI with no further round-trips" and needed no new `Event` variant.

### Fix 2: fan the mono source out to every sink port (`engine/src/session.rs`)

Before, `start_monitor` paired ports positionally with an unconditional `zip()`. After (current tree, `engine/src/session.rs:1550` onward):

```rust
let mut links = Vec::new();
if source_ports.len() == 1 {
    let (out_port_id, _) = source_ports[0];
    for (in_port_id, _) in &sink_ports {
        let props = properties! {
            "link.output.node" => source_node_id.to_string(),
            "link.output.port" => out_port_id.to_string(),
            "link.input.node" => sink_node_id.to_string(),
            "link.input.port" => in_port_id.to_string(),
            // no object.linger: monitoring is temporary (R9)
        };
        if let Ok(link) = core.create_object::<Link>(&factory_name, &props) {
            links.push(link);
        }
    }
} else {
    for ((out_port_id, _), (in_port_id, _)) in source_ports.iter().zip(sink_ports.iter()) {
        // prior positional pairing, truncated to the shorter side --
        // unchanged behavior for a source with more than one output
        // port (not a case this product's own source produces)
    }
}
```

Both port lists are sorted by port name first so positional pairing is stable across differing sink/driver port names.

## Why This Works

**Bug 1 — missing broadcast, not broken mutation.** Confirmed via live raw-IPC testing before any code changed: sending `{"type":"toggle_denoise","enabled":false}` over the Unix socket flipped `dry_mix` from 0.0 to 1.0 both in `~/.config/antibising/config.toml` and in the live PipeWire param (`rnnoise:Dry Mix` via `pw-cli enum-params`). The mutation path was always correct — the entire bug was that `handle_client` sent **zero events** after a successful mutation, not even to the sender. Why that made the UI appear broken: the panel button's click handler derives the next direction from its own last-rendered label —

```js
denoiseToggle.addEventListener('click', () => {
  const nextEnabled = denoiseToggle.textContent === 'Off';
  invoke('toggle_denoise', { enabled: nextEnabled });
});
```

(`app/index.html:353-356`) — and that label is set only by `applySnapshot()` (`app/index.html:265`, reading `snapshot.config.denoise_enabled && snapshot.config.dry_mix < 1`), which is called with Snapshot data from two places: the daemon's `snapshot` event, and the `pull_state` startup-recovery path that caches the most recently observed Snapshot for a webview whose listener registers late. Either way the button's state derives only from an actual `Snapshot`, never from an assumption. With no confirmation event ever arriving after a mutation, the label never changed, so repeated clicks could resend the same `enabled` value forever — indistinguishable from "the button does nothing." Broadcasting a fresh `Snapshot` on mutation closes the loop for this and every future client control that reads state back from `Snapshot` (the threshold slider follows the same pattern). The `Ok(bool)` gate is what makes this safe: session-scoped requests already have their own confirmation path (`MonitorStarted`/`MonitorFailed`/`MonitorStopped` reach only the requester via a separate session registry) — an unconditional broadcast would have leaked those replies to every other connected client, which is exactly what the "What Didn't Work" section above caught.

**Bug 2 — `zip()` truncates silently, and the length mismatch is the ordinary case here, not an edge case.** `Iterator::zip` yields exactly `min(source_ports.len(), sink_ports.len())` pairs; extra elements on the longer side are dropped with no error, no log, and no effect on the success signal — `MonitorStarted` still fires. This product's own permanent source is generated with `audio.position = [ MONO ]` on its playback node (`engine/src/fragment.rs`), so it always has exactly **one** output port, while an ordinary stereo sink has **two** input ports (`playback_FL`, `playback_FR`). So `zip()` linked only the first pair and silently dropped the second sink port — a guaranteed defect on the default path of the feature as built, not a rare edge case.

## Prevention

1. **Any code path that mutates shared state visible over an event/notification protocol needs a paired notification test, not just a state-mutation test.** The Denoise bug had working mutation logic and a working config-persist path; what was missing was the event that makes the mutation observable to clients — and the UI was specifically built to read back only from that event. In this repo the strategy now has a concrete instance: `command_from_one_client_broadcasts_delta_to_both` in `daemon/tests/ipc.rs` was extended to assert that a `SetThreshold` request produces a `Snapshot` broadcast carrying the updated `config.vad_threshold` on **both** connected clients, not just the sender.

2. **Any code pairing two collections of possibly different length via `zip`/positional index needs an explicit test for the length-mismatch case**, not just the common equal-length case. `zip` is silent truncation by design; nothing in the type system or the success path flags it. This product's own configuration (mono source → stereo sink) is a permanent length mismatch on the feature's ordinary path, so the "common case" *was* the mismatch case — which is exactly why it slipped through. The `source_ports.len() == 1` guard in `start_monitor` encodes the invariant at the pairing site; a unit test that feeds a 1-port source against a 2-port sink's port lists and asserts two links would have caught this before live audio testing was needed.

3. **When verifying a live behavior whose lifecycle is tied to a connection, keep the connection alive across the measurement.** The daemon's disconnect-cleanup (`StopMonitor` on connection drop) tears down exactly the resource under test before a separate observer can inspect it; a one-shot script that connects, sends, reads, and exits produces a false negative. Hold the connection open in the driving process and inspect from another process — or expect the teardown and account for it in the test design.

## Related Issues

None under `docs/solutions/` — this is the first entry in that tree for this repository. (A separate, non-`docs/solutions/` research brief, `docs/ksni-tauri-coexistence.md`, predates this doc and covers unrelated ksni/Tauri tray coexistence findings.)
