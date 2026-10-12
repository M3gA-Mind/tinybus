# Required activation validation

This additive API checks an exact host-selected module identity, bus address,
object path, provided interface version range and method set before activating
the actual LoadedArtifact. The loaded initializer and manifest remain together.
Provisional lazy sidecars are refused, while embedded lazy initialization is
supported. No wire, ABI or legacy load behavior changes.

## Regression evidence

The first baseline run reached the incompatible actual-manifest failure:
setup counter1 instead of0. Its sidecar case lacked a Tokio runtime and is
excluded from behavioral evidence. After the fixture-only correction, both
baseline tests failed: incompatible setup ran and provisional sidecar metadata
was accepted for later activation. Logs: guarded-activation-red.log and
guarded-activation-red-fixture-retry.log under the owning worktree target.

The minimal correction passed those two tests. Five focused tests then passed
identity/address/version/method checks, original initializer use and invalid
requirement refusal without echoing input. A real compiled clock regression
first used the wrong interface version range (1.x instead of the fixture's
package0.1.3), which is a fixture-only failure. After correction, all six focused
tests passed, including actual library refusal then admission, configuration
delivery, confidential invocation and the original SHA attestation.

## Gates and limits

Default and all-feature workspace tests, strict all-target/all-feature Clippy,
workspace build, no-default check, strict rustdoc and formatting passed. Rust1.85 full workspace/all-target/all-feature build and the complete dynamic
fixture suite passed. The latter executed509 library,6 CLI,5 macro,42 SDK tests
and6 doctests (568 total, zero ignored). Instrumented coverage executed562 tests;
39 measured production files passed the existing90% per-file gate, minimum
90.0641%, overall94.8386%. New requirements.rs has68/68 lines covered (100%).
The existing CI exclusions were retained without additions.

All final gates completed successfully against the canonical9062dd8 base.
Logs: guarded-activation-{allfeatures,default,clippy,build,slim,docs,fmt,msrv,
fixtures-all,coverage}.log and guarded-activation-coverage.json under target.
This tracked summary retains counts if disposable artifacts are removed.
Host consumption remains contingent on canonical upstream availability.

Platform library constructors may execute during loading before a manifest can
be inspected. This API prevents incompatible ABI setup/configuration callbacks;
it is not process isolation. Existing artifact/directory trust and digest checks
remain authoritative. A host still needs the module-specific immutable instance
mode handshake before sending credentials.

The existing scan-then-load path remains compatible. Hosts that need the new
guarantee must use the new API rather than infer retained admission from a
ModuleInfo returned by scan_dir. No production module release is performed by
this change.
