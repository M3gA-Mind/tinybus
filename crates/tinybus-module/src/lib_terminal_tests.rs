use super::*;
use std::sync::atomic::AtomicUsize;
use tinybus::module::abi::TbSlice;
unsafe extern "C" fn send(_: *mut c_void, _: *const u8, _: usize) -> i32 { TB_OK }
unsafe extern "C" fn notify(_: *mut c_void) {}
unsafe extern "C" fn fault(_: *mut c_void, _: *const u8, _: usize) {}
unsafe extern "C" fn log(_: *mut c_void, _: u32, _: *const u8, _: usize) {}
fn host() -> TbHostVtable {
    TbHostVtable { size: size_of::<TbHostVtable>() as u32, _reserved: 0,
        host_ctx: std::ptr::null_mut(), send, wake: notify, log, ready: notify, fault,
        config: TbSlice { ptr: std::ptr::null(), len: 0 }, broker_routing: None, native_terminal_state: None }
}
fn output() -> TbModuleVtable {
    TbModuleVtable { size: size_of::<TbModuleVtable>() as u32, _reserved: 0,
        module_ctx: std::ptr::null_mut(), deliver, shutdown, reinitialize: None }
}
#[test]
fn native_query_is_required_before_terminal_factory_evaluation() {
    let calls = AtomicUsize::new(0);
    let code = unsafe { start_terminal_reconfigurable_module_with_factory::<serde_json::Value, _, _>(
        &host(), &mut output(), 1, true, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok((|_: Connection, _: serde_json::Value| async { Ok(()) },
                RuntimeHooks::new(|_| {}, || TerminalCompletion::PhysicallyStopped)))
        }) };
    assert_eq!(code, TB_CLOSED);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "missing native query must leave the factory untouched");
}
