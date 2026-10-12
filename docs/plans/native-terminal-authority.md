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
        request: Arc<dyn Fn(TerminalReason) + Send + Sync>,
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

## Task 4: accepted module-local lifecycle protocol

Baseline for this task: a8a8f9ffb935409da5bc3ead20dcad44f7299292.
The corrections below supersede conflicting earlier proposal text.

# Task 4 proposed concrete module ownership

Evidence-only proposal for root preflight. No source/build/pin/ref/commit mutation.
Uses accepted fe080 ABI tail and Task 2/3 host ownership, not another ABI.
Source observations: lib.rs RuntimeState/start_module_runtime/factory-mode/shutdown;
connection.rs::handle_call invokes caught panic handler after dispatch unwinds and
before error enqueue; registry locks are released before user invocation. Existing
transport send faults only after successful send; this cannot remain primary fault
notification for opted-in modules. Existing entrypoints retain their behavior.

## Proposed public module-local API

New terminal.rs, terminal_tests.rs and lib_terminal_tests.rs; lib.rs reexports:

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum TerminalReason { Stop, HostClosed, Fault, SetupFailed }
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum TerminalCompletion { PhysicallyStopped, Uncertain }
    #[derive(Clone)]
    pub struct NativeInstanceLiveness { inner: Arc<TerminalState> }
    pub struct RuntimeHooks {
        request: Arc<dyn Fn(TerminalReason) + Send + Sync>,
        finish: Box<dyn FnOnce() -> TerminalCompletion + Send>,
    }
    impl RuntimeHooks {
        pub fn new<R, F>(request: R, finish: F) -> Self
        where R: Fn(TerminalReason) + Send + Sync + 'static,
              F: FnOnce() -> TerminalCompletion + Send + 'static;
    }
    impl NativeInstanceLiveness {
        pub fn is_live(&self) -> bool;
        pub fn request_fault(&self);
    }

All fields private; no raw pointer, constructor, serde or Connection enrollment.
New terminal_setup_factory macro/linked arm takes factory(view) ->
tinybus::Result<(F, RuntimeHooks)>, F: Fn(Connection, C)->Fut + Send+Sync+'static,
Fut: Future<Output=Result<()>>+Send+'static. Same Arc<F> serves setup/reinit.
New doc-hidden start_terminal_reconfigurable_module_with_factory and linked
equivalent own C argument validation; no change to old setup/setup_factory APIs.

Normalize/copy HostCalls only through trusted unsafe module-init contract. Check
native_terminal_state exists and returns exactly TB_NATIVE_TERMINAL_LIVE before
factory evaluation; unavailable/terminal returns TB_CLOSED with factory untouched.
Do not infer lifetime from broker_routing or sender JSON. Strong callback lifetime
is already part of accepted unsafe ABI contract; actual HostContext persists.
Every is_live samples local terminal, actual callback, local terminal again. Unknown
flags fail closed, mark observed HostClosed locally; valid Fault upgrades Fault.
StopIntent observes Stop. Observation does not claim physical fence/join/backing Drop.

Factory constructs controller/resources but launches worker only after returned
hooks are registered, during setup. Before return, factory owns partial Err/panic
cleanup. A factory calling view.request_fault before returning persists terminal
intent; after hooks register replay one bounded request and deny setup. Factory
Err/panic with no hooks cannot give SDK a join handle it never received.

## Request protocol, cause precedence and non-recursion

TerminalState holds copied HostCalls; monotonic terminal/fault atomics; mutex
ControlState with request Arc Option, separate finish Option, RequestState
(Unregistered/Idle/Running/Done/Panicked), FinishState
(Idle/Anchored/Running/Complete/Uncertain), startup-publication state; Condvar.
All locks recover poison to terminal/uncertain rather than second panic. Atomic
intent is committed before taking these locks or invoking arbitrary code.

Cause precedence: real Fault dominates Stop/HostClosed. SetupFailed is diagnostic
fault cause, also sets fault flag; never downgrade fault after once request. Stop
and HostClosed both normal terminal, no HostCalls::fault. Preserve first real fault
cause; reason passed to already-invoked request is immutable, persisted reason may
upgrade later. Request contract is identical atomic admission closure for every
cause, so fault escalation never needs a second request invocation.

request_fault calls HostCalls::fault FIRST, then records local fault and runs
request_if_registered. Host callback schedules only, never awaits/reenters module
hooks. Host intent closes independent worker phases even if module request cannot
yet run. request_if_registered clones request Arc and marks Running under lock,
releases lock, catches callback unwind, reacquires lock to publish Done/Panicked
and notify. If another caller sees Running it returns without waiting; only
physical finish waits request-done. Request panic commits fault/Uncertain and
notifies BEFORE invoking host.fault for escalation; no retry or recursive request.
Pre-registration request stays pending; registration installs both callbacks under
one lock then invokes request_if_registered outside it. No missed early fault.

Request MUST be bounded atomic admission-close/cancel signal: no I/O/join/reentrant
SDK API, no waiting physical phase gate or Registry/reinitializer mutex. The SDK
cannot forcibly bound trusted callback code; state docs/tests establish contract.
Already granted Publish may finish; actual next-phase query denies immediately.
Physical gate fence plus actual worker join occurs only in finish. Arbitrary worker
catch invokes request_fault after unwinding out of physical gate, not panic hook.

## One retained physical finish owner including no RuntimeState paths

Use private module-global anchor collection of Arc<TerminalState>, one entry per
opted-in instance while controller ownership needs retention. Insert BEFORE
factory evaluation/transfer can create SDK-owned hooks and before any spawn.
Each instance ControlState has ONE optional Arc<FinishJob>; job owns terminal
state plus startup/runtime cleanup resources. Collection owns state independently
of rejected spawn closure, setup future, service refs and output vtable. No public
fake constructor; no extra HostContext or per-view/per-query allocation. This is
explicit module-local controller/anchor bookkeeping, not an ABI lifetime object.
The collection is inspectable/testable and clears completed entries; uncertain
entries deliberately remain. No global boundedness claim; finite total admission
would require separately approved cap. Do not substitute an untracked Box leak
or self-cycle for this external retained owner.

finish scheduling under ControlState: install job then mark Anchored exactly once;
clone job into Builder::spawn. Spawn Err drops only clone, retains original anchor,
marks Uncertain, retains resources and closes intent. No implicit retry/claim reset.
Spawner injectable privately. Actual worker waits request-done AND startup ownership
publication before taking finish. Neither request caller nor runtime worker waits.
Startup locks/ControlState/phase/reinitializer locks all released before finish.
Take FnOnce once under lock, then run outside lock/catch_unwind. Physical completion
requires TerminalCompletion::PhysicallyStopped; failure/panic stays Uncertain and
retains job/state/resources conservatively. Cleanup thread never calls host shutdown.

This additional module cleanup thread exists once per terminal instance, not once
per error; request itself never spawns. C shutdown observes/schedules same job then
waits completion on host's physical blocking worker. A fault during stop cannot
claim second finish. Module startup failure has identical finish owner even when
no valid vtable was ever published and host cannot call shutdown.

## Startup ownership guard: exact transfer and failure paths

Create StartupGuard outside catch_unwind after argument/config/capability validation
and TerminalState/anchor creation. Factory Err/panic before returning hooks marks
terminal, removes empty anchor only when SDK owns no controller/resources; factory
partial disposal remains its responsibility. On successful factory return register
hooks immediately under guard, before tracing subscriber/runtime creation.

Guard owns shared StartupResources (runtime Option, reinitializer slot Option,
connection/inbound resources as created). Every resource is attached to this owned
structure immediately; no fallible gap leaves runtime/closure on unwinding stack
with accidental blocking Drop. Only cleanup worker takes them after publication.

Refactor opted-in runtime builder path, preserving legacy calls:

1. tracing set_global_default failure after hook transfer: guard records SetupFailed,
   host fault, bounded request, schedules anchored finish; returns TB_CLOSED.
2. runtime Builder failure: same, runtime absent. No direct finish on init callback.
3. panic in reinitializer construction, task spawn, RuntimeState construction or
   after ownership transfer: outer catch retains guard/StartupResources, marks fault,
   schedules same finish and returns TB_PANICKED. Runtime Drop occurs on cleanup
   thread after finish, not unwinding ABI/runtime worker.
4. RuntimeState Box created: guard still owns resources until output publication.
   write_module_vtable only uses prevalidated writable capacity. Once complete,
   mark PublishedRuntime and disarm guard, keeping anchor until actual finish.
5. Panic before output publication: no TB_OK/usable output promised; guard publishes
   StartupFailed/resources to job, no host cleanup assumption. After successful
   publication, RuntimeState/shutdown own normal routing and shared finish job.

Guard Drop never joins/blocks: it marks terminal and transfers to already anchored
resources/job, signals publication; catch path performs bounded request/schedule
outside unwind. Guard is fallback when unwinding, not an arbitrary-callback panic
hook. Startup publication wait prevents fault-triggered finish from overtaking
registration/runtime transfer. No await while holding startup locks.

## Runtime shutdown and setup result classification

RuntimeState adds optional terminal state; old entrypoints None unchanged. Opted-in
shutdown first samples actual host flags to select Fault/SetupFailed precedence;
if still Live and actual shutdown entered, commit local Stop. Request once.
Take reinitializer under its mutex, release mutex, drop captured callback outside
lock. Close inbound. Publish cleanup resources; schedule same finish owner.
Wait request/finish completion on host physical shutdown worker, holding no module
runtime/reinitializer/startup/phase locks. Actual Host lifecycle remains serialized
around opaque C callback, as accepted Task 2/3. Runtime.shutdown_timeout runs ONLY
after successful join result, on physical cleanup thread or host worker after
resources ownership is taken once. Uncertain finish returns TB_PANICKED, never TB_OK;
resources remain retained if releasing could discard actual uncertain ownership.

Setup monitor: actual Ok(Err) or JoinError::is_panic upgrades SetupFailed/Fault even
if ordinary stop races. JoinError::is_cancelled while persisted Stop/HostClosed
shares normal cleanup WITHOUT host.fault; successful setup's intentional pending
task canceled by ordinary runtime shutdown is covered. Unexpected cancellation
while Live is SetupFailed. Fault host flags always dominate local normal cause.
Record cancellation decision before runtime teardown; monitor cannot be sole
controller owner because shutdown can abort it. Setup monitor schedules, never
awaits physical finish; failed callback disposal on physical worker releases its
mutex before joining and never blocks runtime needed by in-flight setup/reinit.
Setup/reinit reject terminal before start and before activation. Real setup
Err/panic must be reported at source catch/result boundary before publication of
normal shutdown success; do not rely solely on a monitor canceled during teardown.

Caught method handler installed with __set_panic_handler invokes request_fault
before content-free error construction/enqueue. Existing handle_call clones/drops
handler lock; dispatch already unwound Registry locks. Remove opted-in reliance
on successful ModuleTransport::send panic-error handling; old behavior unchanged.
Global panic hook remains location-only, no terminal hook/join/reentrant SDK code.

## Host init output interaction

No TB_OK from startup failure, and no host invocation of misleading rejected output.
Host PublishedRejected/NoCallbackRequired concerns usable host callback access;
it DOES NOT certify no module startup controller/backing remains. The module's
independent anchor owns failed-startup finish. If finish pending/Uncertain, host
native query stays terminal without PhysicallyStopped. Successful startup finish
with no valid returned vtable still cannot manufacture a host physical-success bit;
only host accepted-vtable shutdown success sets that ABI bit. No extra ABI needed.
Malformed foreign module output/launch without returned hooks remains unjoinable
uncertainty; this safe API never promises to clean resources it never received.

## Scoped red/green tests and actual compiled fixtures

Sibling tests pin unavailable query before factory, retained copied view after
runtime shutdown, independent instance/factory isolation, unknown flags deny;
fault before registration -> exactly one request after registration/no setup;
request races finish with barriers -> finish starts only after request done;
request panic -> host scheduling once/no recursion/no second request;
poison recovery -> terminal uncertain/no double panic;
spawn failure -> real controller/resource Drop stays retained after no-vtable
startup error, Host Drop and runtime teardown; finish FnOnce never repeats.

Inject tracing-registration conflict, runtime-build error, panic after hook transfer,
panic after runtime creation/before output publication; observe actual finish/join
off init/runtime workers and correct uncertainty on spawn failure. Test normal
initial setup cancel and successful pending-task cancel; race real Err/panic with
stop -> Fault preserved; block outbox/host send -> caught handler still closes
native authority before error enqueue. Reinit terminal never launches/replaces.

Actual compiled module_terminal fixture registers std worker/controller, holds
Publish lease and opaque reinit/init, exposes init-bound test C release controls.
Host fault during opaque hold denies next worker query before cleanup can enter;
old lease release enables actual fence/join. Deadline/drop retains join, hook
request bounded while gate held, actual worker panic after unwind, startup failure
with no valid vtable still finishes through module anchor, post-HostDrop query
tombstone safe. Real old-prefix host rejects only opted-in factory; legacy works.
Use barriers/events, no sleeps; compiled controls never become production authority.

No tests infer confidential last-Drop/refund from callback/future/join counters.
No codec/crypto/retirement/global load cap addition. Root preflight must approve
module-global anchor bookkeeping and StartupResources ownership transfer before
Task 4 writer implements; they are explicit proposed internal ownership, not
pretended current APIs or an additional C lifetime contract.

# Task 4 module-local lifecycle preflight

Reviewed accepted-task4-proposed.md against unchanged tinybus-module/src/lib.rs and connection.rs; Task 1 additive ABI now exists. Read-only: only this ignored report written. No source/build/ref/pin mutations. No extra C ABI or allocator/lifetime handoff is needed.

## Important findings and accepted concrete resolutions

**New global collection needs finite admission.** Proposal:93–104 adds a collection retaining failed-startup/Uncertain instances indefinitely. Existing RuntimeState leakage does not account for this: tracing/build failures return before its allocation (lib.rs:570–579,608–615 versus 684–691). A growing Vec plus a global-bound disclaimer is insufficient.

Root accepts a fixed **256** private terminal-instance permit/anchor array reserved before factory evaluation/SDK hook ownership. Exhaustion returns TB_CLOSED without invoking factory. Each state owns its SlotLease; external slot owns Arc<State>, state owns one Arc<Job>, Job holds Weak<State>, and a submitted worker captures strong State+Job. No state/job strong self-cycle. Successful actual finish plus all SDK-owned resource teardown permits anchor/job removal under bookkeeping locks, with destructors outside locks. The slot permit is released only at actual State last-Drop, so retained old liveness views keep their slot occupied and cannot cause unbounded retired-state accumulation through slot reuse. Uncertain/spawn failure retains slot/state/job/resources permanently; no reset/retry/refund. Factory failure before SDK resource transfer can remove its empty anchor; an escaped retained view still keeps the permit occupied until actual Drop. Fixed metadata does not bound arbitrary trusted closure/resource bytes or all mapped SDK images/host contexts. This is an admission cap for the new opt-in bookkeeping, not a broad legacy loader rewrite. Test capacity exhaustion, retained-view permit retention, actual success retirement and uncertain permanent occupancy.

**Startup attachment must include all owned captures, not just Runtime.** Proposal:130–133 mentions runtime/slot/connection/inbound resources but factory returns F before runtime construction. Current factory path builds initial_setup and reinitialize_factory on the ABI stack (lib.rs:927–932,970–971); tracing failure can drop those captures before RuntimeState exists. Root accepts immediate attachment of returned Arc<F>, parsed config/initial setup closure, reinitializer factory/slot, transport/connection/inbound resources and actual task handles to preexisting StartupResources before subsequent fallible/unwind boundaries. Successful returned Runtime must be attached via poison-recovering, non-user-code transfer immediately after Builder success at lib.rs:608–615, before channels/reinitializer/task construction at 616–631. RuntimeState and FinishJob share this resource owner rather than transferring exclusive Runtime to a fallible local stack. No Runtime Drop on ABI/runtime worker; startup failure cleanup runs on anchored physical worker. Arbitrary factory partial cleanup/destructors before hook transfer remain trusted factory obligations; the SDK cannot promise to control resources it never received. Tests inject failure after every transfer boundary and observe actual destructor thread and retention, not merely flags.

**Request failure must not spend physical finish.** Proposal:79–80 says request panic commits Uncertain, but FinishState Uncertain must not prevent the still-uninvoked finish callback from executing. Use orthogonal sticky_uncertainty plus finish invocation state. Request panic/poison publishes fault, marks RequestState Panicked/done and sticky uncertainty, then schedules the same still-eligible physical finish. Never repeat request or consume finish merely because uncertainty was recorded. After actual finish, uncertainty stays visible; no TB_OK/permit refund follows a failed request merely because join succeeded. A failed spawn remains unsubmitted and retained with no automatic retry. Tests cover request panic before job submission and while finish waits request-done.

## Accepted setup/reinit protocol: settle execution before physical completion

Current source keeps the successful initial setup task pending forever (lib.rs:646–655), and its monitor owns the exclusive JoinHandle (664–665). Err/panic is reported only after callback disposal and host fault at 669–680. RuntimeState shutdown disposes callback under the reinitializer mutex then destroys runtime (375–400); C reinit holds that mutex while block_on polls setup (404–417,955–960). A new job that waits this mutex or initial setup completion without first arranging cancellation can wait forever for work whose completion needs terminal cleanup. Joining a worker before an initial/reinit setup future is settled also lets that future launch work after the claimed join.

Use this concrete opted-in-only protocol:

1. Keep initial setup invocation and its returned future in a real task. Catch both synchronous F(connection, config) construction and asynchronous future polling inline, not only the later monitor. Publish actual Err/panic -> Fault/SetupFailed immediately at that result boundary, before readiness or any task-completion notification. Record cancellation separately, preserving already-committed Stop/HostClosed. Existing non-opted-in behavior is unchanged.
2. Initial task returns after its actual setup result; do not park it forever. The already-persisted connection_slot owns the serving Connection (lib.rs:648–649); StartupResources owns that slot for runtime lifetime. FinishJob/StartupResources owns the task's sole JoinHandle. A monitor may observe shared published outcome, but must not consume the only handle required for real cancellation/completion proof.
3. On terminal request, keep Runtime alive, abort an unfinished initial task, and await its actual JoinHandle on the physical cleanup thread (via retained runtime handle). This proves future cancellation/drop has actually happened. Classify is_cancelled caused by Stop/HostClosed as normal; genuine source-observed Err/panic always upgrades Fault. Do not await that join on a runtime worker, under reinitializer/startup/control/gate locks, or after shutting down its runtime.
4. Opted-in reinit drives its setup invocation under terminal-aware cancellation (a terminal notification/predicate select with inline result/panic recording; poll the result branch first when result and terminal are both ready, so an actual ready Err is not lost). This lets a persisted request close the running reinit future while C still owns its mutex; it then returns a normal terminal code and releases that mutex. A real Err/panic observed while polling still faults before returning. No new setup invocation starts after terminal, and readiness/owner publication rechecks terminal. Cleanup can then take the reinitializer slot, release its mutex and drop captured callbacks outside it. Waiting a reinit that ignores cancellation synchronously retains pending ownership; it is not quiescence.
5. After actual initial-setup settlement and reinitializer disposal, run the one finish callback, keeping Runtime services available for the actual controller worker's fence/join. The request callback must already have performed bounded cancel/wake; finish must not rely on an initial/reinit setup future that has been canceled, nor await host shutdown. This dependency contract belongs in public callback docs. Synchronous never-returning user setup, blocking destructor, nonreturning reinit, or worker dependency that cannot settle remains retained pending/Uncertain with no successful completion. Do not fake a timeout as join.
6. After finish success, teardown SDK-owned runtime/connection/callback/task resources exactly once off locks on the physical worker. Only then publish completion and retire the anchor if there is no sticky uncertainty. Recheck local/actual host fault when answering C shutdown; normal Stop cannot erase genuine Fault/SetupFailed arriving during setup or teardown. Fault/uncertain never earns the host physical-success bit. Successful no-vtable startup cleanup proves only module-local cleanup: host query stays terminal and cannot fabricate accepted-vtable shutdown evidence.

This avoids the concrete initial-task/reinitializer/runtime dependency cycle without adding a C field. It changes only the opt-in internal setup task lifetime. If implementation chooses another protocol, it must prove the same actual future settlement before finish/physical success and retain a sole actual JoinHandle or equally strong completion owner; a setup monitor canceled by runtime teardown is insufficient.

## Boundaries already supported by source

Caught per-connection panic handler runs after registry-free dispatch catches unwind and before error enqueue: connection.rs:1441–1453; object-tree lock ends at 1584–1587 before invocation. Transport writing is a separate writer task (1355–1365). Put host fault first in that handler, before error construction/send. ModuleTransport::send's existing TB_OK-only fault at lib.rs:270–285 cannot be relied on for the opted-in path. Request callback is separate Arc; finish FnOnce is separately taken, callback storage locks released before arbitrary code, request completion notified even on panic. Global panic hook remains location-only. A factory pre-fault is persisted before registration, request replayed once after registration, and setup denied; finish waits the startup-publication predicate to avoid overtaking resource transfer.

Task 1's actual host query field docs (abi.rs:184–202) cover copied callback/context and post-runtime worker lifetime. Module-local anchors never export Rust traits/Arc or allocations through C and add no new host context. PhysicalStopped means actual supported shutdown/join evidence, never receiver-last-Drop, codec retirement or foreign allocation destruction.

## Scoped verdict

**Task 4 IMPLEMENTABLE** with this report's accepted corrections governing the writer. Root has accepted fixed256 actual-last-State-Drop admission, job Weak ownership, immediate StartupResources transfer, the initial-task real JoinHandle protocol, result-first terminal-aware reinit cancellation, actual source Err/panic publication, off-runtime request/initial-task settlement/finish/resource-teardown ordering, and final local/native Fault recheck. This is a concrete module-local protocol with no remaining source contradiction or extra ABI requirement.

The required orthogonal matrix is: Request Panicked/done -> sticky_uncertainty, while uninvoked Finish remains eligible for its same once job; actual finish return is recorded independently. SpawnFailed retains anchored unsubmitted owner, sticky uncertainty, no retry. Finish panic/Uncertain retains remaining anchored resources and permit. Only actual successful finish AND all SDK-owned teardown with no sticky uncertainty can remove anchor/job; permit itself remains until State last-Drop. Cause Fault is independent of whether resources were actually joined/released and never disappears from terminal state. A successfully cleaned no-vtable startup instance cannot make the host physical-success bit appear. NoCallbackRequired for no returned callable host vtable must never imply that a registered module controller does not exist.

Arbitrary synchronous nonreturning setup/Drop, blocked opaque reinit or worker dependency can retain pending ownership indefinitely. Callback docs must require bounded cancel/wake request and finish's independent gate/fence/join responsibilities; there is no forced trusted-code cancellation, no arbitrary captured-byte bound, and no confidential receiver destruction proof. Existing legacy entrypoints remain unchanged. Fresh actual compiled failure/cancellation/join/admission proofs remain implementation acceptance gates, not preflight claims. This verdict does not change the active host Task2/3 writer's scope.


## Binding panic-payload order addendum

Root accepts request panic/poison sticky uncertainty independently from the once finish state. The writer must also prevent primary panic payload Drop before the actual fault fence. `catch_unwind(...).is_err()`, `Err(_)`, or temporary matches can dispose the caught `Box<dyn Any + Send>` before later fault notification. Use explicit `Err(payload)` ownership in every new catch. Do not format/downcast payloads or override the global hook.

For once-only module request/finish and the new setup/reinit catches, use fixed private per-instance first-payload cells inside admitted TerminalState, rather than an unbounded vector or a local value whose destructor runs at return. Exact new-source categories: request (once), finish (once), initial setup (first construction/poll panic), reinit (first panic, terminal then denies further invocation), factory (first/only call), startup wrapper (once), and cleanup/SDK teardown wrapper (once). **At most seven first-payload Box owners per admitted instance**, separately named cells, with no arbitrary payload-byte bound. These cells can be omitted where a boundary is already captured by another category, but must not alias categories that can both fail. Initial/reinit construction and polling share their respective first-fault cell; once terminal they cannot be polled/retried to accumulate new primary payloads. This is not a bound on arbitrary panics in unrelated user tasks or legacy/concurrent method calls.

On primary capture: retain ownership explicitly; publish actual host/native Fault plus local terminal/sticky uncertainty without invoking any foreign destructor, then install the payload in its still-empty fixed cell under poison-recovered control storage. For request panic, set Request Panicked/done and notify completion outside held locks BEFORE any potential payload disposal; finish remains eligible once. For setup/reinit publish the real fault before task/C result completion; for finish/teardown record uncertain completion and notify before return. Atomic native/local intent must not depend on acquiring payload storage. No replacement/take-and-drop of a populated cell on a retry: terminal/once admission forbids that retry.

The minimal safe disposal policy is **do not dispose captured primary payloads in this slice**. Sticky-fault state remains externally anchored with its permit/controller/resources; the fixed cells retain payloads for the process lifetime. This is accountable finite owner-count retention under fixed256 admitted states, not an untracked per-panic leak, a zeroization guarantee, or a total foreign-byte bound. The same real physical finish is still attempted once after request completion; sticky uncertainty prevents TB_OK, success-bit publication or permit refund even if worker join succeeds. If implementation instead chooses guarded disposal, its secondary-payload owner must also have an explicitly reserved fixed cell and fault/done notifications must already have happened; arbitrary recursive destructor panic/block/abort cannot be promised recoverable. Do not add a generic panic framework or a resubmission loop.

Captured payload ownership does not make arbitrary user future/capture destruction safe: synchronous blocking Drop or secondary unwind during cancellation can leave the actual task JoinHandle pending or fail. Keep the accepted runtime/job/permit owner alive and do not claim completion. A destructor may abort the process; no generic abort/foreign allocator recovery is promised. Initial-task actual join and reinit exclusion remain required before physical completion; runtime timeout/future cancellation is not their substitute.

### Actual method-panic source gap requiring a narrow opt-in catch observer

Source review found a stronger prerequisite than the prior report's handler-only wording. connection.rs:1554–1557 catches a user future poll with `Err(_) => Poll::Ready(Err(()))`. Its foreign payload can be dropped *inside that catch match*, before `handle_call` invokes the registered panic handler at 1451–1453. Therefore installing fault-first logic only in `__set_panic_handler` is too late for a payload that blocks/panics on Drop.

Add a narrowly private opted-in catch-time terminal observer to the Connection/CatchUnwind path (Rust-local only, no C ABI). Clone its callback outside handler/registry locks. On `Err(payload)` invoke the observer to commit actual host/local Fault, bounded request completion and sticky uncertainty BEFORE payload is dropped; then produce the content-free method error. Existing legacy observer-absent behavior can remain. If observer-triggered request fails, its own fixed request cell/completion protocol applies. Ordinary method dispatch and global panic hooks must not acquire controller joins/gates.

Do not pretend all concurrent method panic payloads fit the seven once-boundary cells: method calls have no such count bound. The narrow observer guarantees fault-before-disposal for those payloads; it does not promise to retain/recover every arbitrary method destructor. After observer commits sticky fault, a blocking/secondary-panicking method payload can prevent later error delivery/task settlement, but native authority is already denied and the actual external owner/permit remains retained. No physical success/refund follows that method panic. Guarding and repeatedly retaining arbitrary method payloads would require separately bounded admission and is outside this slice.

**Addendum verdict:** module-local once-boundary retention is implementable within fixed admitted state and without new ABI. Full Task4 fault-before-payloadDrop proof additionally requires the narrow opted-in Connection catch-time observer above; handler-only implementation does not pass. This is a precise source-backed correction, not a promise about all legacy connection panics. Add a panic_any payload with blocking Drop barrier: verify independent native query denies and request-done is observable before the barrier is released; repeat request/finish/initial/reinit boundary cases and assert the actual once finish attempt/retained permit rather than flags alone.


Root accepts the narrow opt-in Connection catch-time prerequisite. Its observer is installed only by the actual terminal factory/runtime path, closes private TerminalState plus actual HostCalls::fault first, and catches request panic through the fixed request-cell/done protocol before the primary method payload can drop. No public fake liveness constructor, payload-selected observer authority or connection-wide growing collection is introduced. New setup construction/poll/reinit catches use the same terminal observer ordering. Preserve existing content-free MethodFailed wire schema and legacy observer-absent behavior.

Runtime resource qualification is binding: `runtime.shutdown_timeout` (current lib.rs:393) can return with opaque blocking tasks/Drop still alive. Its return is not proof those owners were destroyed. Remove successful anchors/release accounting only with actual teardown evidence. Any surviving SDK task/transport/observer must retain its actual TerminalState clone, keeping the State-owned permit occupied through actual task/clone Drop; if that ownership cannot be established, keep the external Uncertain anchor. Never interpret timeout, async Connection closure or callback/future drain as actual method-task Drop, independent-worker join or receiver last-Drop. The controller's proved worker join is a separate fact from SDK-owned resource destruction. User-created detached tasks not received/tracked by SDK remain the trusted controller's lifecycle obligation; no generic task-destruction guarantee follows.

Exact source touchpoints/tests for this correction: `tinybus/src/connection.rs::CatchUnwind::poll` (1554–1557) explicit payload capture/observer before Drop, `handle_call` (1441–1453) observer cloning outside lock/content-free error after fault, and `tinybus-module/src/lib.rs` caught setup/runtime wrappers plus new terminal.rs request/finish first-payload cells. Controlled `panic_any` payload records Drop entry and may block there: assert actual native Fault observed before Drop entry, actual Request done notification before waiting finish, and outgoing blocked/failed send cannot postpone the fence. Request/finish primary payload is retained in its fixed cell and never reaches Drop in this slice; method payload may drop only after the observer fence. Check actual original once finish attempt, sticky uncertainty and held permit while task/destructor/native join remains pending. Do not infer task destruction from future-count or message-drain acknowledgements.

**Final scoped Task4 verdict: IMPLEMENTABLE with all binding corrections in this report, including the accepted narrow opt-in catch-time observer and actual SDK-owner/permit retention qualification.** Implementation acceptance still requires fresh native/source proofs. No source, builds, refs, pins, global hook changes or foreign payload formatting were performed for this review.

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
