# Immutable signal provenance

Signal delivery carries transport/publication identity, never tenant authority.
`SignalDelivery` seals a protocol message with `SignalContext` before entering
its bounded broadcast queue. No header, body field, Hello response or serde
operation can construct a verified context.

## Delivery paths

- `Brokered`: the existing private per-frame broker proof establishes a unique
  `authenticated_sender`. Real in-memory broker deliveries and the module SDK
  trusted-native proof use the same admission boundary as `CallContext`.
- `Local`: `Connection::deliver_local` captures that connection's opaque
  `LocalPublisherIdentity`. It never authenticates a header or Hello name.
- `Unverified`: direct public sends, wrapped/custom transports and Unix socket
  delivery have no verified identity. Completing Hello does not change that.

Identity is scoped to its captured connection/broker lifetime. Replacing a
connection creates a new local identity; connection clones preserve it. A host
must explicitly admit the exact local identity or broker peer in its own trusted
initialization. An identity is not permission and is not a durable restart key.

## Compatible handlers

Existing `EventHandler::handle` implementations retain their behavior. The
additive `handle_with_context` defaults to `handle`; an authority-dependent
handler overrides it and refuses unverified or unadmitted publishers before
using application state. Domain filtering, cancellation, panic isolation and
bounded lossy broadcast remain unchanged.

`Connection::signal_deliveries` exposes sealed deliveries. Legacy `signals` and
`add_match` receivers still expose `Message` without any authority-bearing
context, so existing consumers need no conversion. Subscribe before installing a
broker match rule to avoid a receive gap. Both queues are independently bounded;
lag remains observable rather than gaining an unbounded replay buffer.

## Host boundary

For tenant routing, transport/local proof is only one prerequisite. The host
must also retain opaque captured producer/job authorization and validate its
current lifetime, cancellation and replay rules. Generic `Local`, plaintext
tenant fields, and ambient subscriber task context are insufficient. This
change adds no tenant metadata, broker payload parsing or host policy.

## Regression evidence

Baseline typed events: 24 passed. The new real broker/local tests initially
failed behaviorally (one passing, three failures, exit 101): typed subscriber
lost broker proof and local publisher identity. The direct forged-sender frame
remained unverified. Sealing at ingress fixes those failures. Additional tests
cover canceled receive, queued verified then raw unverified delivery, custom
transport after Hello, unchanged legacy handler defaults, exact admitted local
publisher refusal for an identical remote payload, and opaque identity cloning.
