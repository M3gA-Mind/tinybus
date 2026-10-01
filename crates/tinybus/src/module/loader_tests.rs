use super::*;

#[test]
fn descriptor_gate_rejects_invalid_size_and_endianness_declarations() {
    let path = Path::new("module.so");
    let mut descriptor = TbAbiDescriptor::current("module", crate::VERSION);
    descriptor.descriptor_size = DESCRIPTOR_PREFIX_SIZE - 1;
    assert!(gate_descriptor(path, &descriptor, false).is_err());

    let mut descriptor = TbAbiDescriptor::current("module", crate::VERSION);
    descriptor.flags ^= 1 << 2;
    assert!(gate_descriptor(path, &descriptor, false).is_err());
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
