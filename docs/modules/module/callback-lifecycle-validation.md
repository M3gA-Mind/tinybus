# Captured callback lifecycle validation

This follow-up uses canonical `HostCallbackBinding`, introduced by PR 56, on
canonical main `1f2557657db97470cd4efb7e1fc2f374f36fdd22`. It introduces no second
callback capability or serialized authority. The earlier unpublished ModulePeer
branch and its tests remain preserved as historical work; they are not part of
this change or its acceptance evidence.

## Regressions

The native cancellation case pauses the real ABI reinitializer, cancels its
public waiter, attempts enable/disable while the native callback remains
blocked, releases the callback, and verifies a subsequent queued reinitialize.
Baseline run 45344 executed one test and failed because disable succeeded while the
native callback still ran. Corrected run 25116 executed one test and passed.

The compiled-module case loads a hash-verified module through the actual
`load_file_with_host_callbacks` API. Its captured interface pauses after
authenticated confidential admission and subsequently commits. Single polling
of stop observes actual native shutdown admission before releasing that work;
there are no sleeps or inferred scheduling assumptions. Baseline run 90374 ran the
owning E2E gate: nineteen artifact cases passed and this case failed because
native shutdown started before callback commit. Corrected run 82695 passed all twenty
artifact cases, plus the existing helper, admission and umask controls.

## Implementation

The loaded record and transport retain one exact `CallbackState`, including
operation accounting, after routing revocation removes its route. The account
is bound before callback admission and cannot be replaced. Its backlink to the
transport remains weak.

Stop revokes routing and commits native stop intent before waiting for admitted
user futures with its remaining deadline. This closes native admission without
submitting physical shutdown. Drain timeout or public waiter cancellation keeps
stop intent pending; enable cannot restore admission. Once the actual provider
account drains, stop submits the existing physical owner. Ordinary stop intent
is not upgraded to a fault.

Last-host drop also uses that physical owner. Its OS worker revokes the exact
account and waits for its actual running count to reach zero before acquiring
the native lifecycle mutex or calling opaque shutdown. This wait has no runtime
dependency or deadline. A provider that never releases its future keeps the
physical job pending. Provider futures are destroyed before their running
accounts decrement, including when their runtime is destroyed. No provider code
or destructor runs under the accounting mutex.

Reinitialize retains its marker in the actual blocking callback until it
returns. Enable/disable also checks committed stop intent and the retained
physical job, including uncertain cleanup. Public waiter completion or timeout
is not evidence of physical reclamation.

## Upstream acceptance before integration

Run 87340 completed with exit 0 on source
`372a0ddd97d23daf58203d59b95b7be79fc3393a`. All 3,068 output lines were inspected:

- Default workspace: 514 bus, 5 macro and 42 module cases passed.
- Existing feature gate: slim checks; 508 bus cases with defaults disabled;
  all features 522 bus, 5 CLI, 5 macro and 42 module cases passed.
- Strict all-target/all-feature workspace Clippy and strict all-feature rustdoc
  passed, as did formatting and diff checks.
- Rust 1.85 all-target/all-feature build and the actual compiled E2E gate passed,
  including all twenty artifact cases with no artifact ignores.
- The unchanged coverage gate passed: 542 bus, 6 CLI, 5 macro and 42 module cases;
  every reported production file met 90% line coverage. Total line coverage was
  95.10%; callback implementation 92.91%, transport 91.85%.

An independent read-only review of the corrected source and full regression
logs found no remaining authority or lifecycle defect before acceptance.

## Limits

The drain covers admitted user futures, including native port calls awaited by
them. It does not account for tasks detached by user code, undo an admitted
transaction, prove distributed profile fencing, or establish a public typed
storage bus. A response lost after a commit remains an unknown outcome. Existing
frozen callback configuration and reinitialize binding semantics are unchanged.
No OpenHuman implementation, gitlink update, merge or release is included.
