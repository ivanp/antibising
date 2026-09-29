# Concepts

Shared domain vocabulary for this project — entities, named processes, and status concepts with project-specific meaning. Seeded with core domain vocabulary, then accretes as ce-compound and ce-compound-refresh process learnings; direct edits are fine. Glossary only, not a spec or catch-all.

## Permanent Source

A single audio source with a fixed, device-independent identity that exists for the lifetime of the user session, not the lifetime of any physical microphone. It exists before any microphone is connected, stays up while microphones are swapped, and survives after the last one is removed — the whole point being that applications holding it never need to reacquire audio when the underlying device changes.

The Permanent Source's channel width is fixed at mono, with an explicit per-channel downmix gain rather than the platform default — an unstated downmix sums channels at unity gain, which clips ordinary stereo input.

## Snapshot

The daemon-to-client event carrying the complete current state — every known device, the current Health verdict, and the persisted configuration — sufficient for a client to render its entire UI without any further round-trip. Sent once on connect and once whenever a client-initiated request actually changes persisted configuration. An explicit re-sync request refreshes the daemon's own cached state but does not itself produce a client-visible Snapshot — only a connect or a mutation does. Distinct from a *delta* event (device arrived/departed, health changed), which carries only what changed. A client is expected to derive its rendered state from the most recent Snapshot it has seen, never from an assumption about what a request it just sent must have done — a control that reads its own prior rendered state instead of waiting for a Snapshot will desync silently the moment a Snapshot fails to arrive.

## Health

The daemon's honesty-constrained verdict on whether the Permanent Source is actually usable right now. States: linked to a specific live device and confirmed flowing; present but silently fed by no device (a legal, non-error state — not every moment has a microphone); broken, with a reason precise enough to act on; or reconnecting, a transient distinct from broken because it is an expected consequence of the underlying audio system restarting, not a failure requiring self-repair. The distinguishing rule across all states: Health must never claim more than the daemon has verified — a plausible-looking connection is not the same as a confirmed one, and the verdict must say so.

## Generation

One PipeWire connection-and-object-graph epoch owned by the session loop: every device, port, and link discovered or created while connected to a given PipeWire core instance belongs to exactly one Generation and is rebuilt entirely from scratch — never patched — on every reconnect, because PipeWire assigns new object identifiers on each connection and a prior Generation's identifiers can never be reused or reconciled. This clean-rebuild rule is why Health's *reconnecting* state is a transient, not a failure: it is the expected cost of starting a fresh Generation, not evidence something broke.

A Generation's *routing policy* — which device is preferred or pinned, and the active denoise parameters — is deliberately not Generation-owned: policy is user intent, and unlike PipeWire object state it must survive every reconnect rather than reset with it. Conflating the two — resetting policy alongside object state on every rebuild — silently strands the Permanent Source with no device chosen until a user re-selects one by hand.
