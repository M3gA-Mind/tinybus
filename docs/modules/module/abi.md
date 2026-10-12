# Module ABI v1

Rust has no stable ABI. The module boundary therefore carries only `repr(C)`
plain data, opaque pointers, borrowed byte slices and `extern "C"` callbacks.
No Rust allocation, trait object, future, Tokio type, panic payload or
`repr(Rust)` value crosses it.

The tail of the host vtable contains a borrowed JSON configuration slice. It is
valid for the init call; the SDK deserializes it into module-owned memory before
returning. Configuration decode failures return a numeric status and never
include attacker-controlled values in host logs.
The host zeroes and releases its serialized copy immediately after init
returns; modules must not retain the borrowed pointer.
The module vtable's optional tail callback accepts replacement configuration.
The host bounds the call, zeroizes its serialized buffer afterward, and treats
older vtables that end before the callback as modules without reinitialization
support. The host initializes the output vtable's leading `size` field with its
allocation capacity before calling init. A newer module reads only that frozen
field first and writes no more than the advertised capacity, so the additive
tail is safe in both compatibility directions.
The legacy SDK bounds reinitialization setup to four seconds and returns `TB_TIMEOUT`
after cancelling the setup future. The host serializes this callback with
shutdown and retains the lifecycle lock even if its own outer deadline expires.
The final host callback, `ready`, marks completion of asynchronous setup. This
keeps a lazy module's reserved name routable during initialization without
announcing ownership before its objects are serving.

## Optional host capability tails

ABI revision 1 and the original host prefix remain frozen. Each optional field
is available only when `size` reaches that field's offset plus its complete
field size. The `broker_routing` callback therefore remains available from an
older routing-capable host even though it lacks the newer
`native_terminal_state: Option<unsafe extern "C" fn(*mut c_void) -> u32>` tail.
A null host or a size below the frozen prefix is rejected; absent and partial
optional fields normalize independently to `None`.

The native query contract admits only exactly Live (`1`). Unsupported queries,
zero, unknown bits, combined values, StopIntent (`2`), Fault (`4`) and
PhysicallyStopped (`8`) deny native admission. Terminal intent is monotonic
and never returns to Live. Query success linearizes admission; a cached result
cannot authorize a later operation, and a query alone grants no publication
authority. Later intent cannot undo an already admitted operation.

PhysicallyStopped means successful opaque shutdown return. For modules opting
into native terminal hooks it additionally requires explicit finish proving
independent worker join. It proves neither legacy worker completion nor receiver
backing destruction. Faulted or uncertain shutdown must not earn this bit.

A module copies the normalized callback and context, never borrows the host
vtable or configuration storage. A host supplying the query must keep that
callback and context valid and thread-safe for every retained view use,
including independent workers after runtime shutdown or host destruction.
Host destruction must leave a terminal tombstone; this context must never be
recycled or reopened. Custom unsafe C hosts must truthfully uphold this
contract, which cannot cryptographically authenticate an in-process host.
This tail defines an optional ABI capability. Unsupported hosts supply `None`;
terminal factories reject them before factory evaluation. It introduces no
retain/release ABI or aggregate resource bound: the existing process-lifetime
host-context and library retention is unbounded.

Opted-in SDK instances use the existing opaque `module_ctx` field for a private
slot/generation address token. No callback dereferences it and no ABI field or
context allocation is added. Each SDK image has 256 slots; callback lookup
clones the exact matching State under registry exclusion, then releases that
lock before instance operations. Successful resource teardown may retire the
anchor, but slot reuse waits for actual State last-Drop, including retained
liveness views and SDK resources. Generation exhaustion permanently denies the
slot instead of wrapping. Unknown or retired tokens return Closed; an old
instance token cannot act on a replacement. These tokens are not serialized or
logged and do not authenticate a native host.

Terminal reinitialization uses result-first polling when completion and terminal
intent are simultaneously ready. Cancellation leaves its foreign future owned
by the instance until physical cleanup. Its runtime and terminal captures are
destroyed on the independent physical cleanup thread. Uncertainty remains
sticky and cannot be refunded by a timeout or shutdown return. Genuine native
Fault is sampled even after local Stop, and faulted published instances retain
their context anchor instead of earning successful retirement.

## Symbols

Every module exports exactly these revisioned symbols:

- `TINYBUS_MODULE_ABI_V1`: a data descriptor;
- `tinybus_module_manifest_v1()`: borrowed JSON manifest bytes;
- `tinybus_module_init_v1(host, out)`: initialization after admission.

Putting the revision in each name makes an old module fail symbol lookup before
the host interprets a changed layout.

## Descriptor gate

The first 16 bytes are frozen as four 32-bit fields: magic, ABI revision,
descriptor size, and flags. The host rejects a wrong prefix, sizes outside
16–4096 bytes, and descriptors smaller
than the v1 structure before reading more. Larger descriptors are accepted and
their tail ignored.

Admission then requires matching pointer width, endianness, target triple, a
compatible tinybus series, and module feature bits that are a subset of the
host's. Modules built with `panic=abort` are refused. A rustc mismatch warns in
permissive mode and refuses in strict mode.

Fixed byte arrays hold target, toolchain and identity fields. Displayed values
keep only ASCII letters, digits, `.`, `_`, `+`, and `-`, capped at 32
characters.

## Frames and runtimes

Both directions pass one borrowed JSON `Message` document without the socket
transport's four-byte length prefix. The 16 MiB frame cap applies at both ends.
The receiver copies the bytes before returning from the callback.

Each module creates its own Tokio runtime. A statically linked library has a
separate copy of Tokio's thread-locals, so it cannot borrow the host runtime.
The host-to-module `deliver` callback only performs a bounded `try_send` and
never blocks a host thread. Queue capacity is reported as backpressure and the
host retries after the module's `wake` callback.
Before shutdown, the physical lifecycle owner closes initialization and delivery
admission and discards queued messages. It then waits for every reserved
`deliver` callback to return before entering the opaque shutdown callback.
Each backpressure retry requires a new reservation. This barrier covers actual
callback execution, including signals; it does not wait for unanswered method
calls or detached module work. A caller timeout or cancellation retains the
physical worker, callback reservations, and lifecycle lock. A callback that
never returns prevents teardown rather than allowing shutdown underneath it.
Ordinary admission closure is distinct from a module fault and from physical
shutdown completion. An already reported Ready snapshot remains until physical
completion; late `ready` callbacks cannot reopen admission.
The symmetric module-to-host `send` callback applies bounded backpressure by
blocking the calling module thread until host queue capacity is available.
Modules must not call it from a thread that has to remain responsive.

The SDK installs a panic hook that forwards only the source location, never the
payload, and faults the transport so the broker releases the module's names.
Native faults remain outside what an in-process boundary can contain.
