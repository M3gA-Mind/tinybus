# Native instance terminal authority implementation plan

Status: proposed implementation; root reviews before source implementation.
Baseline: 4daf0d4fa1ab3afdc30fcc7c09b6e9d6243cd4f5; canonical comparison 1f25576.
Branch security-bus-7328, existing upstream PR #57. Preserve complete PR scope:
actual callback drain, stop admission, physical lifecycle lock, original stop
task/transition markers and ordinary non-Fault stop semantics. This document is
not TinySec App #263 acceptance or issue #7328 completion.

Use existing superproject worktree only. All shell commands explicitly cd to
/home/enamakel/work/openhuman/worktrees/security-bus-7328/vendor/tinybus.
No nested worktree, second SDK PR, release, gitlink advance or host workaround.
Auto-commit checkpoints stay separate; never squash.

## Contract and bounds

Add optional safe per-instance terminal factory in tinybus-module and a size-gated
ABI-v1 host query. Rust closures/traits/Arc/worker handles stay module-local.
Existing C shutdown owns serialized physical cleanup and actual join.

Separate terminal intent, physical gate fence, worker join and backing last-Drop.
Callback future drain, Connection closure, host deadline and runtime shutdown
prove no independent worker quiescence. Already admitted Publish may complete;
next phase must deny after terminal intent. Module physical gate checks immutable
owner scope/generation/receiver, local flags, live native query, then local flags
again. Query success is admission linearization; later intent cannot undo an OS
operation already granted. Query alone is not a publication permit.

Normal stop commits intent only after acquiring actual lifecycle lock, before
handing it to retained physical task. Canceled/unacquired stop makes no commitment.
Actual fault, rollback and Host Drop commit immediately, even behind opaque C init
or reinit. Fault takes public-state precedence; normal stop never becomes Faulted.
Fault cleanup must not create ordinary transition_from or restart terminal modules.

ModuleTransport::new already leaks HostContext and stores a static reference;
loader never dlcloses. Copy normalized callback plus context, never borrowed
vtable/config storage. New view adds no context allocation/retain-release ABI.
Context is terminal tombstone after Host Drop, never recycled/reopened. Callback
contract covers retained independent worker uses after runtime shutdown.
Custom unsafe C hosts must truthfully uphold this lifetime/issuer contract; the
SDK cannot authenticate a malicious in-process host cryptographically. Payloads,
foreign metadata, serde IDs and cached Live receipts never construct authority.

Existing process-wide context/RuntimeState/library residue is unbounded. This
addition must qualify that preexisting gap; no bounded aggregate claim follows.
If strict new native admission requires a global bound, separately review finite
process-lifetime context quota reserved before exposure, retained after exposure,
with bounded per-slot dynamic content. No speculative pool rewrite or slot reuse.
One initializer/cleanup per instance is a distinct bound from total load count.
No crypto, confidential backing codec or retirement protocol enters this slice.

## Proposed safe Rust surface (new names, not existing API)

New crates/tinybus-module/src/terminal.rs and terminal_tests.rs; lib.rs reexports:

    enum TerminalReason { Stop, Fault, SetupFailed, HostClosed }
    enum TerminalCompletion { PhysicallyStopped, Uncertain }
    struct NativeInstanceLiveness { inner: Arc<TerminalState> }
    struct RuntimeHooks {
        request: Box<dyn Fn(TerminalReason) + Send + Sync>,
        finish: Box<dyn FnOnce() -> TerminalCompletion + Send>,
    }

Fields private. RuntimeHooks::new accepts those callback bounds plus static.
NativeInstanceLiveness is Clone, has is_live() -> bool and request_fault().
No public constructor, serde, raw pointer access or arbitrary Connection enrollment.
request_fault first HostCalls::fault, then local monotonic fault plus bounded
request outside panic hook, registry mutex and module phase gate. Never joins.
Independent worker catch_unwind invokes it after unwinding out of physical gate.
Host-origin faults are observed immediately through query even while local request
is serialized behind opaque lifecycle code.

New macro arm terminal_setup_factory = factory, config = C, with linked equivalent.
Safe factory takes NativeInstanceLiveness and returns Result<(F, RuntimeHooks)>.
F: Fn(Connection, C) -> Fut + Send + Sync + static;
Fut: Future<Output = Result<()>> + Send + static.
Validate host/output/count/config and capability before factory evaluation.
Create TerminalState before factory; store one immutable controller after return.
Factory constructs controller but launches worker only during initial setup after
registration. Before return, factory owns partial-failure disposal. SDK owns it
after return. Same Arc<F> serves setup/reinit; activation checks view before
publishing owner/service. No hook replacement or terminal reset on reinit.

Private TerminalState holds copied HostCalls, monotonic AtomicU32 intent,
AtomicBool request/finish claims, mutex Option<RuntimeHooks>, atomic completion.
Extract hook under lock, release lock, catch_unwind invoking. Poison recovery
must not create a second panic. Request panic closes/faults, no retry/refund;
finish panic leaves Uncertain. Callback is contractually bounded/no I/O/no join/
no reentrant SDK calls: arbitrary trusted Rust cannot be forcibly made bounded.
Physical finish joins actual controller-owned worker and retains charged resources
until owner-defined actual destruction. SDK never infers destruction from counters.

## Task 1: additive independently checked ABI tail

Files: crates/tinybus/src/module/abi.rs, abi_tests.rs;
crates/tinybus-module/src/lib_tests.rs and adjusted host-vtable literals.

Write failing tests first: routing_tail_survives_newer_optional_tail,
terminal_tail_requires_its_complete_field, prefix_hosts_normalize_both_tails.
Actual repr(C) prefix and old-routing structs allocate only declared bytes.
Cover null, below prefix, exact prefix, partial routing, complete old routing,
partial new callback, full None/Some, and larger storage. Run focused tests red.

Append native_terminal_state: Option<unsafe extern C fn(*mut c_void) -> u32>.
ABI constants Live=1, StopIntent=2, Fault=4, PhysicallyStopped=8. Exactly 1 admits;
zero/unknown/combined/terminal flags deny. Terminal flags monotonic, never Live.
Physical bit means successful opaque shutdown return, and for opted-in modules
explicit finish proved worker join. It proves neither legacy worker completion
nor receiver backing destruction. Fault/uncertain shutdown never earns this bit.

Normalization independently checks offset_of(field) + sizeof(field type) for
broker_routing and new callback; never sizeof enlarged whole vtable for old tail.
Frozen prefix/base size and ABI version stay unchanged. Document stronger callback
lifetime covering every retained view use. Run focused tests green.

## Task 2: retain real deferred initialization before deadline abandonment

Files: crates/tinybus/src/module/transport.rs and transport_tests.rs,
host.rs/host_tests.rs, host_callbacks.rs/host_callbacks_tests.rs.

Existing DeferredInitializer::run returns SendModuleVtable through spawn_blocking;
ensure_initialized timeout drops JoinHandle, so eventual vtable can disappear
before self.initialize validates/stores it. Current stop_sync rollback can also
overlap opaque init. Necessary correction stays in these exact paths.

First red test late_initialization_retains_real_vtable_for_cleanup: actual blocked
initializer, controlled host future timeout/drop, retained native worker/query,
release init returning valid vtable, exactly one real shutdown/join, no new phase.
Add rollback_cannot_overlap_opaque_initialization and late_invalid_vtable_never_runs.

Retained blocking initializer owns Arc transport, actual owned lifecycle guard,
DeferredInitializer (and config borrow), resident library assumptions and eventual
vtable. Install via existing initialize validation before result publication to
separate waiter channel; JoinHandle result carries no exclusive physical owner.
Validate size/header/version/callback validity under existing initialize contract;
never invoke invalid/missing callbacks or fabricate a controller from partial ABI.
Failure with no usable returned vtable retains honest uncertainty for a foreign
initializer that launched work without returning cleanup; native query still denies.

Host deadline/drop does not drop result owner or borrow. Timeout commits native
fault intent immediately, makes public failed/fault state terminal, and retains
one eventual cleanup. Clear config only after actual C return. Publish validated
vtable even when terminal, then release lifecycle guard and claim/schedule cleanup
once. Terminal vtable storage is cleanup access, never public resurrection.
No new initializer retries or unbounded spawn_blocking loops: one physical init,
one claim for cleanup; caller retries only observe same result/tombstone.
If init never returns, cleanup pending and real ownership survive indefinitely.

## Task 3: actual host flags, fault cleanup and original stop invariants

Same host files. Red tests fault_closes_native_phase_before_reinitializer_returns,
unacquired_stop_keeps_native_instance_live, abandoned_stop_keeps_original_cleanup,
host_drop_leaves_terminal_query_tombstone, and
fault_during_stop_keeps_fault_transition_and_single_shutdown.

HostContext adds AtomicU32 terminal flags; commit_terminal clears Live and sets
StopIntent plus Fault when real fault. Query only Acquire loads leaked actual ctx,
no locks/allocation/parsing/product code. Install callback in ModuleTransport::new.
HostContext immutable Weak<ModuleTransport> from Arc::new_cyclic enables host_fault
to schedule cleanup without retaining strong context cycle.

Separate physical cleanup claim from existing stop_task_spawned. Normal stop
acquires lifecycle then claims/commits before spawn. Fault immediately commits,
then private schedule_terminal_cleanup retains Arc and queues one serialized
physical cleanup. Retained std thread using blocking_lock_owned permits Host Drop
cleanup independent of dropped Tokio runtime; never recurse into C shutdown from
host_fault, opaque callback, router/loaded lock or Tokio worker. Thread spawn failure
keeps terminal/uncertain ownership; no successful completion/reopen/refund.
Normal running task and fault share claim, never second join. stop_task_spawned
still denotes actual ordinary stop task, preserving StopError Started/NotStarted.

Route host_fault, fault_pending, failed initialization/send, admission rollback
and Host Drop through same intent/claim. Gather cloned transports under lock,
commit intent before detach, release loaded/router/inbound locks before scheduling.
Remove synchronous rollback shutdown under lifecycle-bypassing Drop path.
Before-init rollback closes init gate without pretending worker joined.

peer_detached checks actual fault before consuming normal transition_from;
private fault cleanup never creates that origin. Late stop_inner success preserves
actual Faulted/Failed precedence. Ordinary finish_stop stays non-fault. Keep every
existing #57 callback lease/drain/cancel/retry/original marker assertion unchanged.

## Task 4: module-local hooks through runtime and fault paths

Files: new terminal.rs/terminal_tests.rs/lib_terminal_tests.rs in tinybus-module;
lib.rs, lib_setup_factory_tests.rs, lib_lifecycle_tests.rs.
Declare sibling test modules, no inline tests.

Red tests unavailable query rejects before factory; two instances isolated;
retained view after runtime shutdown; host fault precedes local request;
request/finish once; poisoned hook storage; finish panic stays uncertain;
terminal reinit never reruns setup; retained service refs cannot suppress cleanup.

Implement API above. New private start_terminal_reconfigurable_module_factory_mode
and doc-hidden linked/unlinked entrypoints serve macro arms. Existing entrypoints
retain old behavior. start_module_runtime takes optional Arc<TerminalState>;
transport, RuntimeState and setup monitor share it, existing callers pass None.
shutdown requests Stop before callback disposal, releases reinitializer mutex,
then runs finish synchronously on retained physical host cleanup worker before
runtime.shutdown_timeout. Indefinite join outlives deadline without TB_OK.
Uncertain/panic finish returns content-free failure, never physical completion.

Caught method panic publishes host fault even if error-frame send fails, then
bounded request outside registry lock. Determine actual send lock boundary before
coding: if lock is held, retained controller task runs request after host intent.
Setup failure publishes host intent before waiting callback disposal, request and
finish run off runtime workers. Setup canceled by shutdown takes same once route.
No joining/hooks in global panic hook (location logging only). setup/reinit reject
terminal immediately and before activation. Never classify ordinary close as fault.

## Task 5: actual compiled native proof and compatibility negatives

New declared cdylibs crates/tinybus/examples/module_terminal.rs and
module_legacy_prefix.rs, explicit examples in crates/tinybus/Cargo.toml.
Legacy fixture uses locally frozen repr(C) prefix, not enlarged current layout.
Extend scripts/check-coverage.sh build/env validation and host_tests.rs loader
tests. Use existing staged_module hash allowlist/private copies; separate mapping
per test avoids module-global tracing collision. Include all fixtures in packages.

Fixed test interface controls worker phase, held Publish, held reinit/init,
queued jobs, worker panic, failure before/after launch, receiver retention/Drop.
Use std channel/Condvar barriers and event acknowledgements, never sleeps.
After fault bus detaches: test-only exported C control functions, bound from the
actual staged library during init, release barriers and read actual completion.
Do not make production liveness constructors or payload control authority.

Required red/green actual compiled tests:

1. Hold opaque C reinit/init; actual host fault denies independent worker's next
   physical phase before releasing opaque call. Cleanup stays pending, then once.
2. Old Publish lease survives stop timeout/caller cancellation; new phase denies;
   original callback drain/task/lock and join retained. Release proves real join.
   Retained receiver last-Drop is separate and never inferred from future drain.
3. Actual method/worker panic and poisoned bounded local gate: one close/finish,
   denied queued callers, no blocking panic hook/reopen/refund.
4. Init/factory failure before launch, after registration/launch, rollback and
   eventual valid vtable after abandoned waiter preserve real cleanup ownership.
5. Retained actual view after Host Drop/runtime shutdown queries terminal tombstone;
   two instance isolation, no stale reuse or terminal reset on reinit/retry.
6. Compiled old module/new host; new terminal module/actual compiled prefix-only
   host rejects before factory, ordinary legacy setup still loads. Truncated and
   tampered prefix negatives must allocate genuinely frozen layout. JSON Live,
   foreign metadata/serialized IDs cannot fabricate query or admission.

Fixtures prove SDK lifecycle only, not TinySec charge or confidential last-Drop.

## Task 6: docs, declarations and complete proof gates

Update docs/modules/module/abi.md and README.md with actual optional tail/lifetime,
factory example, bounded request, physical join, post-runtime tombstone, legacy
denial and preexisting unbounded residue. Public Rust docs match actual API.

Long gates use absolute superproject scripts/ci-cancel-aware.sh from explicit SDK
cwd. Never export CARGO_TARGET_DIR or send builds into temp directories.
Run focused red/green first and retain commands/full outputs/exit status:

- cargo fmt --all -- --check
- cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
- cargo test --locked --workspace --all-features; preserve baseline 557 assertions
- cargo check --locked --workspace --all-targets --all-features
- cargo check --locked --package tinybus --no-default-features
- scripts/test-features.sh and Rust 1.85.0 enabled/disabled all-target checks
- cargo test --locked --workspace --doc --all-features
- Build all declared cdylibs, execute native ignored tests with explicit fixture
  paths from both SDK cwd and superproject cwd via manifest path; fresh staging
- Pack tinybus/macros/module with compilation verification and real local registry
  dependency strategy; --no-verify alone is not package compile proof
- Fresh scripts/check-coverage.sh, at least 90% meaningful compiled file lines,
  no new ignore regex. Separate llvm-cov JSON verifies at least 80% actual changed
  executable lines versus current canonical and full PR base, including excluded
  host integration code in a dedicated report; no global percent substitution
- OpenHuman rust:layout/crate-chain/feature forwarding with native/module ABI off
  and on, complete fixture/declaration checks and actual old/new ABI execution

Review exact final diff and independently verify fresh proof before root updates
existing upstream #57 ready for review. No merge/release/pin/issue closure granted.

## Source-backed review decisions before implementation

This is an exact interface/ownership/TDD plan, not a fabricated complete source
replacement. Prove init/normal-stop/fault cleanup-claim handoff, actual method-panic
registry boundary, thread-spawn failure retention and invalid-init-vtable contract.
If source makes these contradictory, escalate exact conflict before green code;
never invent quiescence or silently weaken immediate native fault denial.
Resolve separately whether bounded global native admission is required: leaked
context pointer safety cannot satisfy it. No additional ABI lifetime allocation
is necessary for the approved actual SDK context contract.
