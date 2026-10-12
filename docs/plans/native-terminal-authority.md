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
nor receiver backing destruction. The generic host sets the bit only after an
accepted opaque callback actually returns TB_OK; StopIntent and Fault remain
monotonic and continue denying admission (a legacy fault cleanup can return
flags 14). This is not opted-in hook acceptance. For new native terminal hooks,
TB_OK additionally requires actual independent worker/SDK resource finish and
no sticky uncertainty or fault, under the separate Task 4 protocol.

Normalization independently checks offset_of(field) + sizeof(field type) for
broker_routing and new callback; never sizeof enlarged whole vtable for old tail.
Frozen prefix/base size and ABI version stay unchanged. Document stronger callback
lifetime covering every retained view use. Run focused tests green.

## Task 2: retain initialization and publication independently of waiters

Files: crates/tinybus/src/module/transport.rs and transport_tests.rs,
host.rs/host_tests.rs, host_callbacks.rs/host_callbacks_tests.rs; declared
module_controlled_init native fixture and its existing CI staging.

Use one mutex-protected PhysicalState in the existing process-lifetime
HostContext. InitState is Deferred, Scheduled, Entered, PublishedValid,
PublishedRejected, CanceledBeforeEntry or ExternalValid. CleanupState is Idle,
Requested, Executing, Complete, Uncertain or NoCallbackRequired. These are
orthogonal states: Requested does not consume the shutdown invocation, and an
empty vtable slot never proves physical success.

Before thread submission, install one externally anchored Arc<PhysicalJob>
owning Arc<ModuleTransport>, at most one initializer, and any acquired owned
lifecycle guard. The context anchor survives rejected/dropped spawn closures,
Host Drop and Tokio runtime teardown. Never retry failed submission or reset
Uncertain. This is a finite per-instance ownership bound, not a global leak
bound. Result predicates use Notify and Condvar loops, not notification counts.

The non-async scheduling helper takes initialization-gate then PhysicalState,
rechecks terminal admission, takes the initializer once and records Scheduled
before awaiting physical completion. OnceCell does not own initializer scheduling
or eventual vtable publication. Only the initiating waiter has an armed Drop
guard: deadline expiry or abandonment before publication immediately commits
Fault and Requested. Observation waiters cannot cancel the physical owner.
Already-committed ordinary cancellation does not invent Fault.

The retained std thread acquires the actual owned lifecycle guard without
holding host state locks. Before C entry it briefly checks PhysicalState:
terminal Scheduled becomes CanceledBeforeEntry/NoCallbackRequired without any
opaque call; live Scheduled becomes Entered. Entered retains config until real
C return regardless of a host deadline or dropped waiter. After C return,
validate only the existing contract: module size >= base size, non-null context,
and Some(reinitialize) if the full module tail is declared. No fictitious
header/version validation, null testing of mandatory Rust function pointers,
or executable-address authentication is added. Non-OK output is ignored;
structurally rejected output invokes no callbacks and retains Uncertain.

Store accepted vtables under lifecycle exclusion even after terminal intent,
for cleanup-only access. Clear config after the real borrow ends and publish the
immutable result. Under PhysicalState, Requested keeps the SAME initializer
anchor for cleanup; live publication retires the anchor atomically before
lifecycle release; rejection retains uncertainty. Requested cleanup releases
then reacquires lifecycle, never waits for its own guard or submits a second
cleanup worker. Reinit must recheck terminal after obtaining lifecycle.

Eager activation uses the same Entered/publication predicates. Its fresh,
unexposed transport is demonstrably unlocked: try_lock_owned before opaque C
entry, fail content-free/uncertain on unexpected contention, retain caller Arc,
config and guard through actual return and validation/publication, then release
exclusion. Cleanup waits both eager and deferred Entered publication before
acquiring lifecycle. Late accepted eager output is cleanup only and cannot
attach or reactivate. Failed cleanup submission remains Uncertain even if
eager output is valid later; no refund or resubmission.

Prove with controlled channels/Condvar barriers and an actual compiled fixture:
blocked native init followed by initiating deadline/drop and late single join;
fault while Scheduled (C never entered); fault after C return before publication
(same owner, no empty-success claim); actual rejected closure/controller retention;
invalid size/null context/missing declared reinit/non-OK misleading output with
zero callbacks; eager fault while config remains borrowed and late single join.

## Task 3: actual host flags and retained terminal cleanup

HostContext supplies the actual AtomicU32 query through the optional ABI tail:
Live=1, StopIntent=2, Fault=4, PhysicallyStopped=8. Only exactly Live permits a
phase. Query performs an Acquire load on the leaked context without locks,
allocation, parsing or product logic. Weak<ModuleTransport> allows host_fault to
upgrade the actual owner without an unconditional strong transport cycle.

Commit native StopIntent (and actual Fault) immediately before physical cleanup
scheduling. Fault precedence is monotonic. host_fault releases inbound locks
before scheduling and never invokes C recursively, joins or waits. Rollback and
Host Drop commit all flags before revocation, release router/loaded locks and
route through retained cleanup. No lifecycle-bypassing synchronous Drop cleanup.

Callback-load admission carries an explicit Failed versus Canceled cause.
Actual setup Err/unwind, initialization error and actual readiness deadline are
Failed even when ordinary stop races them. After successful setup, waiter Drop,
admission-gate rejection and terminal-caused not-ready/final-commit cancellation
preserve already committed ordinary StopIntent without adding Fault. Live
abandonment still faults; real Fault remains sticky. Apply the selected cause
before revocation and to retained cleanup, including nested observer release.
The final load liveness check rejects actual admission closure even while the
legacy Ready bit remains set and opaque shutdown has not returned.

Bounded cfg(test) admission/decision observers and fixture JSON Controls encode
test barriers only, never production authority. The declared native fixture emits
actual Ready during opaque init when requested. Tests hold actual shutdown return
after the independent worker joins, prove ordinary cancellation flags2 and eventual
TB_OK flags10, and distinguish genuine deadline/actual host_fault flags6/14.
A host setup unwind test proves Rust rollback classification, not module catch-time
panic containment or Task4 hooks. Setup rejection before gate release proves C never
entered and makes no physical-join claim.

Normal stop remains deadline-bound while obtaining the actual owned lifecycle
guard. Before acquisition it owns no ordinary intent, job or stop_task_spawned
marker; cancellation/deadline is NotStarted and remains live absent real fault.
After acquisition commit and install/adopt an actual owner synchronously before
the next await. If an already-submitted initializer is Scheduled, ordinary stop
adopts that SAME job, records the original handoff, releases its acquired guard,
then observes completion; the existing worker obtains lifecycle, sees terminal
and proves CanceledBeforeEntry. Never store the guard where that worker needs it
or create a second worker. A running Entered initializer remains excluded; an
ordinary lock deadline does not cancel it or fabricate Fault.

Fault-owned jobs are observed rather than claimed as new ordinary handoffs.
Original stop_task_spawned and transition_from.get_or_insert(old) survive retry
and caller cancellation. Preserve Started/NotStarted distinctions and ordinary
non-Fault behavior, including an actual non-OK shutdown return. Failed ordinary
submission retains the actual acquired guard in the external anchor and explicit
Uncertain without inventing module Fault or silently retrying.

Cleanup waits initialization publication before lifecycle acquisition, then
rechecks terminal/cleanup state, closes the actual callback gate and drains its
physical leases. Only accepted cleanup access can claim Requested -> Executing.
Hold physical exclusion and retained job until the one opaque shutdown returns.
Only actual TB_OK permits Complete and the physical-success bit; failed return
or panic is Uncertain. Legacy public Stopped is separate bookkeeping and never
proves join or backing destruction. Successful cleanup can release the anchor;
uncertainty remains inspectable, with no reset/restart.

peer_detached checks actual Fault before consuming ordinary transition_from.
Late stop_inner completion preserves Faulted/Failed. Native tests cover blocked
actual reinit fault/query denial before return, ordinary callback lease retention,
Host Drop/rollback tombstones, instance isolation and one real join. Keep every
original #57 callback drain, physical lock, retry and original marker assertion.

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

Task 4 remains separate, unimplemented scope. Its startup guard owns returned
hooks until runtime/output publication; factory Err/panic before return owns
partial disposal itself. Request callback is an independent Arc and finish a
separate take-once slot. Publish intent, claim bounded request outside locks,
signal request completion even on panic; physical finish waits request-done
off runtime, callback-gate and reinitializer locks. Pre-registration fault
persists and invokes bounded request once at registration. Ordinary
terminal-caused setup cancellation must remain non-Fault, while genuine
Err/panic still upgrades Fault even racing ordinary stop.

The accepted host prerequisite does not establish catch-time fault publication,
module-local worker/resource finish, fixed retained job bounds or confidential
payload/backing last-Drop. Task 4's own accepted specification must prove those
contracts at the actual CatchUnwind boundary before payload destruction; no
enqueue/send-success inference, extra panic task or global panic hook join.
A caught registry-free callback boundary must publish fault/request before
returning its error. New physical-success evidence remains distinct from legacy
Stopped and receiver backing destruction.

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
