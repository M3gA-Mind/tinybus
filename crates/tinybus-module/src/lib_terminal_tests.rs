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

unsafe extern "C" fn native_query(ctx: *mut c_void) -> u32 {
    unsafe { &*ctx.cast::<std::sync::atomic::AtomicU32>() }.load(Ordering::Acquire)
}
#[test]
fn shutdown_rechecks_actual_native_fault_after_local_stop_and_physical_finish() {
    let flags = Arc::new(std::sync::atomic::AtomicU32::new(tinybus::module::abi::TB_NATIVE_TERMINAL_LIVE));
    let mut host = host();
    host.host_ctx = Arc::as_ptr(&flags).cast_mut().cast();
    host.native_terminal_state = Some(native_query);
    let state = terminal::TerminalState::admit(HostCalls(host), None).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let release = Arc::new(std::sync::Barrier::new(2));
    let worker_release = release.clone();
    state.register(RuntimeHooks::new(|_| {}, move || {
        entered_tx.send(()).unwrap(); worker_release.wait(); TerminalCompletion::PhysicallyStopped
    }));
    state.publish();
    let ptr = state.context().addr();
    let caller = std::thread::spawn(move || unsafe { terminal_shutdown(ptr as *mut c_void, 3000) });
    entered_rx.recv().unwrap();
    flags.store(tinybus::module::abi::TB_NATIVE_TERMINAL_FAULT, Ordering::Release);
    release.wait();
    let code = caller.join().unwrap();
    assert_eq!(code, TB_PANICKED, "actual native Fault cannot disappear behind local Stop");
}
