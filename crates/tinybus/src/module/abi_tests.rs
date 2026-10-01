use super::*;

#[test]
fn the_current_descriptor_describes_this_build_and_truncates_long_fields() {
    let descriptor = TbAbiDescriptor::current(
        "a-module-name-that-is-deliberately-longer-than-the-fixed-abi-field-can-hold",
        "0.1.0",
    );

    assert_eq!(descriptor.magic, ABI_MAGIC);
    assert_eq!(descriptor.abi_revision, ABI_REVISION);
    assert_eq!(
        descriptor.descriptor_size as usize,
        size_of::<TbAbiDescriptor>()
    );
    assert_eq!(descriptor.pointer_width, usize::BITS);
    assert_eq!(field_bytes(&descriptor.module_name).len(), 64);
    assert_eq!(field_bytes(&descriptor.module_version), b"0.1.0");
}

#[test]
fn field_bytes_stops_at_nul_or_uses_the_entire_field() {
    assert_eq!(field_bytes(b"abc\0tail"), b"abc");
    assert_eq!(field_bytes(b"whole"), b"whole");
}

#[test]
fn an_uninitialized_module_vtable_refuses_calls() {
    let vtable = TbModuleVtable::default();
    assert_eq!(
        TB_MODULE_VTABLE_BASE_SIZE as usize,
        std::mem::offset_of!(TbModuleVtable, reinitialize)
    );
    assert_eq!(vtable.size as usize, size_of::<TbModuleVtable>());
    assert_eq!(
        unsafe { (vtable.deliver)(vtable.module_ctx, std::ptr::null(), 0) },
        TB_CLOSED
    );
    assert_eq!(
        unsafe { (vtable.shutdown)(vtable.module_ctx, 0) },
        TB_CLOSED
    );
}
