---
title: "PipeWire Reconnect Drops Session Routing Policy"
date: 2026-09-17
category: logic-errors
module: engine/session
problem_type: logic_error
component: service_layer
severity: high
symptoms:
  - First app launch always shows "No microphone" even though a mic (Razer Seiren Mini) is present, requiring the user to select "Auto (ranked)" and then re-select the mic to activate it
  - Closing the app (while antibisingd keeps running) silences the mic again, needing the same manual re-selection to recover
  - Health reports SilentNoDevice after any PipeWire generation rebuild even though a preferred/pinned device and RNNoise settings were already configured
  - pw-dump shows zero capture links into antibising_mic_capture immediately after a rebuild
root_cause: logic_error
resolution_type: code_fix
tags:
  - pipewire
  - session-generation
  - reconnect
  - routing-policy
  - rnnoise
  - mic-activation
  - engine-session
---

# PipeWire Reconnect Drops Session Routing Policy

## Problem

When the PipeWire daemon restarted or the engine reconnected to the PipeWire core, audio capture links silently stopped working, leaving the session in a dead `SilentNoDevice` state. The root cause was that `Generation` state in `engine/src/session.rs` was constructed afresh on every connection via `Generation::new()`, resetting routing and filter configuration (`pin`, `preference_order`, `dry_mix`, `vad_threshold`) to empty/default values. Because the parent daemon only sends configuration commands (`SetPreferenceOrder`, `SetPin`, `SetRnnoiseParam`) once upon initial startup, any subsequent reconnect left the new generation without routing directives. When `compute_desired_feed` ran on the reconstructed graph, it evaluated to `DesiredFeed::None`, destroyed existing capture links, and failed to link any input device.

## Symptoms

- After a PipeWire core restart or crash-reconnect cycle, microphone audio input stopped routing to the engine filter chain.
- The engine reported `SilentNoDevice` even though audio input devices remained present in the PipeWire registry.
- `Generation` instances reinitialized with empty `preference_order` (`Vec::new()`) and `pin: None`.
- Log traces showed core error listener handling a disconnect (`id == 0`), successfully establishing a new generation, but failing to link capture ports because `compute_desired_feed` had no device preference or pin configuration.

## What Didn't Work

During initial diagnosis, distinguishing fatal core disconnects from non-fatal object errors was identified directly via live-tracing:
- PipeWire emits core error events where `id == 0` signifies the core context itself (a fatal disconnect requiring rebuild), whereas errors with non-zero IDs (such as `id == 6` or other object IDs) represent non-fatal individual proxy/node errors that should not tear down the connection.
- Direct live-tracing revealed this distinction immediately on the first attempt without false starts.
- Attempting to reconcile stale node and device IDs across reconnects was recognized as an anti-pattern: PipeWire assigns entirely new 32-bit IDs on restart, making clean generation recreation necessary. However, resetting policy state along with the object graph was the actual flaw.

## Solution

To solve this, routing and filter parameters were decoupled from transient graph identity state by introducing `LoopPolicy`. `run_loop` retains a persistent `LoopPolicy` across `Generation` rebuilds, updates the policy whenever configuration commands arrive over the channel, and seeds each new generation via `Generation::with_policy(&LoopPolicy)`.

### 1. `LoopPolicy` Struct

A dedicated `LoopPolicy` struct carries configuration across connection generations:

```rust
/// Routing/RNNoise policy carried by `run_loop` across `Generation`
/// rebuilds. A `Generation` is rebuilt from scratch on every PipeWire
/// (re)connect (see the doc comment below), which used to silently drop
/// `pin`/`preference_order`/`dry_mix`/`vad_threshold` — the daemon only
/// ever sends `SetPreferenceOrder`/`SetPin`/`SetRnnoiseParam` once at
/// startup, so any later reconnect left the new generation with no
/// routing policy at all (`compute_desired_feed` -> `DesiredFeed::None`
/// -> capture links destroyed -> `SilentNoDevice`). `run_loop` owns one
/// `LoopPolicy` for the life of the thread, updates it whenever one of
/// those four commands arrives, and seeds every new `Generation` from it.
#[derive(Debug, Clone, Default)]
struct LoopPolicy {
    preference_order: Vec<DeviceId>,
    pin: Option<DeviceId>,
    dry_mix: f32,
    vad_threshold: f32,
}

impl LoopPolicy {
    /// Same defaults `Generation::new()` used to hardcode inline.
    fn new() -> Self {
        Self {
            preference_order: Vec::new(),
            pin: None,
            dry_mix: crate::fragment::FragmentConfig::default().dry_mix as f32,
            vad_threshold: crate::fragment::FragmentConfig::default().vad_threshold as f32,
        }
    }
}
```

### 2. `Generation::with_policy`

`Generation::new` was replaced in production code with `Generation::with_policy`, retaining `Generation::new` only for test convenience:

```rust
impl Generation {
    /// Test-only convenience: build a `Generation` with default policy
    /// (no pin, empty preference order, fragment defaults for RNNoise).
    /// Production code always goes through `with_policy` so a rebuilt
    /// generation inherits whatever `run_loop`'s `LoopPolicy` holds.
    #[cfg(test)]
    fn new() -> Self {
        Self::with_policy(&LoopPolicy::new())
    }

    fn with_policy(policy: &LoopPolicy) -> Self {
        Self {
            devices: HashMap::new(),
            device_identities: HashMap::new(),
            pending_nodes: HashMap::new(),
            device_output_ports: HashMap::new(),
            capture_node_id: None,
            capture_node_proxy: None,
            capture_input_ports: Vec::new(),
            capture_links: HashMap::new(),
            preference_order: policy.preference_order.clone(),
            pin: policy.pin.clone(),
            dry_mix: policy.dry_mix,
            vad_threshold: policy.vad_threshold,
            last_health: None,
            source_node_id: None,
            source_output_ports: Vec::new(),
            sinks: HashMap::new(),
            sink_input_ports: HashMap::new(),
            default_sink_name: None,
            device_form_factors: HashMap::new(),
            monitor_sessions: HashMap::new(),
            meter_sessions: HashMap::new(),
            raw_meter_sessions: HashMap::new(),
            input_ports_by_node: HashMap::new(),
            port_owner: HashMap::new(),
        }
    }
    // ...
}
```

### 3. Loop Ownership and Core Error Detection

`run_loop` maintains policy state across `run_one_generation` invocations:

```rust
fn run_loop(
    our_source_name: String,
    cmd_rx: StdReceiver<SessionCommand>,
    event_tx: StdSender<SessionEvent>,
) {
    let mut backoff = Duration::from_millis(200);
    const MAX_BACKOFF: Duration = Duration::from_secs(5);

    let mut cmd_rx = cmd_rx;
    let mut policy = LoopPolicy::new();
    loop {
        let outcome;
        let returned_policy;
        (outcome, cmd_rx, returned_policy) =
            run_one_generation(&our_source_name, cmd_rx, &event_tx, policy);
        policy = returned_policy;
        match outcome {
            GenerationOutcome::Disconnected => {
                let _ = event_tx.send(SessionEvent::Disconnected);
                std::thread::sleep(backoff);
                backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
            }
            GenerationOutcome::ConnectFailed => {
                std::thread::sleep(backoff);
                backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
            }
            GenerationOutcome::ShutdownRequested => return,
        }
    }
}
```

The core listener watches specifically for `id == 0` to trigger a disconnect and clean rebuild:

```rust
    // Core error listener: any error callback with id == 0 (the core
    // itself) is treated as a disconnect trigger. PipeWire crash/restart
    // surfaces here.
    let disc_flag = disconnected.clone();
    let ml_quit = mainloop.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, seq, res, msg| {
            if id == 0 {
                tracing::warn!(id, seq, res, msg = %msg, "core error — triggering rebuild");
                disc_flag.set(true);
                ml_quit.quit();
            } else {
                tracing::debug!(id, seq, res, msg = %msg, "core error (non-fatal, ignored)");
            }
        })
        .register();
```

### 4. Updating `policy_cell` in Command Handlers

Inside `run_one_generation`, an `Rc<RefCell<LoopPolicy>>` is shared with the timer-based command polling closure. The three policy-modifying command handlers update both `policy_cell` and the active `generation`:

```rust
    let generation = Rc::new(RefCell::new(Generation::with_policy(&policy)));
    // Carries policy updates (`SetPreferenceOrder`/`SetPin`/
    // `SetRnnoiseParam`) forward to the *next* generation. `Generation`
    // itself is rebuilt from scratch on every reconnect, so this is the
    // only thing that survives a rebuild.
    let policy_cell = Rc::new(RefCell::new(policy));
```

```rust
                Ok(SessionCommand::SetPreferenceOrder(order)) => {
                    policy_for_timer.borrow_mut().preference_order = order.clone();
                    let mut gen = gen_for_timer.borrow_mut();
                    gen.preference_order = order;
                    reconcile_now(&mut gen, &core_for_timer, &link_factory_for_timer, &tx_for_timer);
                }
                Ok(SessionCommand::SetPin(pin)) => {
                    policy_for_timer.borrow_mut().pin = pin.clone();
                    let mut gen = gen_for_timer.borrow_mut();
                    gen.pin = pin;
                    reconcile_now(&mut gen, &core_for_timer, &link_factory_for_timer, &tx_for_timer);
                }
                Ok(SessionCommand::SetRnnoiseParam(param, value)) => {
                    match param {
                        crate::params::RnnoiseParam::DryMix => {
                            policy_for_timer.borrow_mut().dry_mix = value;
                        }
                        crate::params::RnnoiseParam::VadThreshold => {
                            policy_for_timer.borrow_mut().vad_threshold = value;
                        }
                        _ => {}
                    }
                    let mut gen = gen_for_timer.borrow_mut();
                    match param {
                        crate::params::RnnoiseParam::DryMix => gen.dry_mix = value,
                        crate::params::RnnoiseParam::VadThreshold => gen.vad_threshold = value,
                        _ => {}
                    }
                    gen.set_rnnoise_param(param, value);
                }
```

When `run_one_generation` completes, it returns `final_policy` along with the command receiver and outcome:

```rust
    let final_policy = policy_cell.borrow().clone();
    (outcome, cmd_rx, final_policy)
```

## Why This Works

1. **Separation of Lifetime Domains**: Ephemeral connection resources (PipeWire node IDs, port IDs, proxy references, link objects) are strictly owned by `Generation` and discarded on disconnect. Policy configuration (`LoopPolicy`) is thread-scoped and owned by `run_loop`.
2. **Deterministic Seed on Reconnection**: When PipeWire crashes or restarts, the new connection initializes a fresh `Generation` via `Generation::with_policy(&policy)`. The new generation begins with the exact `pin`, `preference_order`, `dry_mix`, and `vad_threshold` settings previously established.
3. **Graph Reconciliation Convergence**: Once the new registry globals arrive, the coalesced reconciler (`reconcile_now`) evaluates `compute_desired_feed` using the preserved policy, immediately identifying the correct source device and establishing capture links.

## Prevention

### Unit / Regression Testing

A dedicated unit test `policy_survives_generation_rebuild` in `engine/src/session.rs` verifies that policy parameters survive across generation rebuilds without being dropped or consumed:

```rust
    #[test]
    fn policy_survives_generation_rebuild() {
        // LoopPolicy must carry pin + preference_order + RNNoise params
        // across generation boundaries: this is the actual fix for the
        // mic-activation-persistence bug — Generation::new() used to
        // reset all four fields to defaults on every PipeWire reconnect,
        // silently dropping routing policy the daemon only ever sends
        // once at startup.
        let order = vec![DeviceId("test_device".to_string())];
        let pin = Some(DeviceId("test_device".to_string()));
        let policy = LoopPolicy {
            preference_order: order.clone(),
            pin: pin.clone(),
            dry_mix: 0.5,
            vad_threshold: 42.0,
        };
        let gen = Generation::with_policy(&policy);
        assert_eq!(gen.preference_order, order);
        assert_eq!(gen.pin, pin);
        assert_eq!(gen.dry_mix, 0.5);
        assert_eq!(gen.vad_threshold, 42.0);

        // A second generation built from the same policy (simulating a
        // second rebuild) still carries it — policy is not consumed.
        let gen2 = Generation::with_policy(&policy);
        assert_eq!(gen2.preference_order, order);
        assert_eq!(gen2.pin, pin);
    }
```

### Review Heuristic

When designing or reviewing stateful connection/session managers that undergo reconnection or lifecycle recreation:
- **Distinguish Object-Identity State from Policy/Preference State**:
  - *Object-identity state* (socket handles, protocol IDs, node proxies, link IDs) belongs to a single connection instance and MUST be discarded and rebuilt cleanly upon reconnection rather than patched.
  - *Policy/preference state* (user pin, priority list, volume/mix levels, DSP thresholds) represents operator intent and MUST survive reconnect/rebuild boundaries.
- **Audit Struct Fields on Full Rebuilds**: When a struct is created with `Default::default()` or empty containers upon reconnect, verify whether any fields receive values from external initialization messages that are not re-transmitted on reconnect. Any such field must be extracted into an outer configuration container.

## Related Issues
- `docs/solutions/logic-errors/daemon-ipc-no-broadcast-after-mutation.md` — different bug in the same `daemon`/`engine/src/session.rs` domain (a missing IPC broadcast after config mutation, plus a mono-to-stereo monitor fanout fix); moderate overlap on referenced files and solution approach, but a distinct root cause and fix.
