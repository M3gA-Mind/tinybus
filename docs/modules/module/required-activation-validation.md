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
workspace build, no-default check, strict rustdoc and formatting passed. MSRV,
the complete dynamic fixture suite and per-file coverage are pending at this
checkpoint. No upstream publication or host consumption is claimed yet.

Platform library constructors may execute during loading before a manifest can
be inspected. This API prevents incompatible ABI setup/configuration callbacks;
it is not process isolation. Existing artifact/directory trust and digest checks
remain authoritative. A host still needs the module-specific immutable instance
mode handshake before sending credentials.

The existing scan-then-load path remains compatible. Hosts that need the new
guarantee must use the new API rather than infer retained admission from a
ModuleInfo returned by scan_dir. No production module release is performed by
this change.
