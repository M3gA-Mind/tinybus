use super::*;

fn refusal(result: Result<impl Sized>) -> String {
    match result {
        Ok(_) => panic!("the loader accepted it"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn descriptor_gate_rejects_invalid_size_and_endianness_declarations() {
    let path = Path::new("module.so");
    let mut descriptor = TbAbiDescriptor::current("module", crate::VERSION);
    descriptor.descriptor_size = DESCRIPTOR_PREFIX_SIZE - 1;
    assert!(
        refusal(gate_descriptor(path, &descriptor, false)).contains("descriptor size is invalid")
    );

    let mut descriptor = TbAbiDescriptor::current("module", crate::VERSION);
    descriptor.flags ^= 1 << 2;
    assert!(
        refusal(gate_descriptor(path, &descriptor, false))
            .contains("target endianness does not match")
    );
}

#[test]
fn the_descriptor_prefix_is_refused_for_each_reason() {
    let path = Path::new("module.so");
    let full = size_of::<TbAbiDescriptor>() as u32;
    assert!(gate_prefix(path, ABI_MAGIC, ABI_REVISION, full).is_ok());
    for (magic, revision, size, reason) in [
        (
            ABI_MAGIC ^ 1,
            ABI_REVISION,
            full,
            "ABI magic does not match",
        ),
        (
            ABI_MAGIC,
            ABI_REVISION + 1,
            full,
            "ABI revision does not match",
        ),
        (
            ABI_MAGIC,
            ABI_REVISION,
            DESCRIPTOR_PREFIX_SIZE - 1,
            "descriptor size is invalid",
        ),
        (
            ABI_MAGIC,
            ABI_REVISION,
            MAX_DESCRIPTOR_SIZE + 1,
            "descriptor size is invalid",
        ),
        (ABI_MAGIC, ABI_REVISION, full - 1, "descriptor is too small"),
    ] {
        let error = refusal(gate_prefix(path, magic, revision, size));
        assert!(error.contains(reason), "{reason}: {error}");
    }
}

#[test]
fn the_manifest_slice_is_refused_for_each_reason() {
    let path = Path::new("module.so");
    let slice = |bytes: &[u8]| TbSlice {
        ptr: bytes.as_ptr(),
        len: bytes.len(),
    };
    let manifest = serde_json::json!({
        "tinybus_manifest": 1,
        "module": {"name": "fixture", "version": "1.0.0"},
        "bus_name": "ai.tinyhumans.Fixture",
        "object_path": "/ai/tinyhumans/Fixture"
    })
    .to_string();
    assert!(unsafe { manifest_from(path, slice(manifest.as_bytes())) }.is_ok());

    let null = TbSlice {
        ptr: std::ptr::null(),
        len: 0,
    };
    // Over the cap is refused before the bytes are read, so no buffer that
    // large has to exist.
    let oversized = TbSlice {
        ptr: manifest.as_ptr(),
        len: MAX_MANIFEST_BYTES + 1,
    };
    for (bytes, reason) in [
        (null, "manifest bytes are invalid"),
        (oversized, "manifest bytes are invalid"),
        (slice(b"{not json"), "manifest is not valid JSON"),
    ] {
        let error = refusal(unsafe { manifest_from(path, bytes) });
        assert!(error.contains(reason), "{reason}: {error}");
    }
}

#[cfg(unix)]
#[test]
fn unix_loader_refuses_missing_and_nul_containing_paths() {
    let missing = Path::new("/definitely/not/a/tinybus-module.so");
    let missing_error = match load(missing, false) {
        Ok(_) => panic!("missing module unexpectedly loaded"),
        Err(error) => error,
    };
    assert!(
        missing_error
            .to_string()
            .contains("dynamic loader rejected the artifact")
    );

    use std::os::unix::ffi::OsStrExt;
    let nul_path = Path::new(std::ffi::OsStr::from_bytes(b"module\0name"));
    let nul_error = match platform::open(nul_path) {
        Ok(_) => panic!("NUL-containing module path unexpectedly loaded"),
        Err(error) => error,
    };
    assert!(nul_error.to_string().contains("artifact path is invalid"));
}
