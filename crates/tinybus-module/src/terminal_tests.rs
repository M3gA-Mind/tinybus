use super::*;
use std::sync::atomic::AtomicU32;
use tinybus::module::abi::{TbSlice, TB_NATIVE_TERMINAL_STOP_INTENT};
static FLAGS: AtomicU32 = AtomicU32::new(TB_NATIVE_TERMINAL_LIVE);
unsafe extern "C" fn query(_: *mut c_void) -> u32 { FLAGS.load(Ordering::Acquire) }
unsafe extern "C" fn send(_: *mut c_void, _: *const u8, _: usize) -> i32 { TB_OK }
unsafe extern "C" fn notify(_: *mut c_void) {}
unsafe extern "C" fn fault(_: *mut c_void, _: *const u8, _: usize) { FLAGS.store(4, Ordering::Release); }
unsafe extern "C" fn log(_: *mut c_void, _: u32, _: *const u8, _: usize) {}
fn host() -> HostCalls {
    HostCalls(TbHostVtable { size: size_of::<TbHostVtable>() as u32, _reserved: 0,
        host_ctx: std::ptr::null_mut(), send, wake: notify, log, ready: notify,
        fault, config: TbSlice { ptr: std::ptr::null(), len: 0 }, broker_routing: None,
        native_terminal_state: Some(query) })
}
#[test]
fn observed_native_terminal_never_reopens_a_retained_view() {
    FLAGS.store(TB_NATIVE_TERMINAL_LIVE, Ordering::Release);
    let view = NativeInstanceLiveness { inner: Arc::new(TerminalState { host: host() }) };
    assert!(view.is_live());
    FLAGS.store(TB_NATIVE_TERMINAL_STOP_INTENT, Ordering::Release);
    assert!(!view.is_live());
    FLAGS.store(TB_NATIVE_TERMINAL_LIVE, Ordering::Release);
    assert!(!view.is_live(), "an observed terminal tombstone must stay closed");
}
