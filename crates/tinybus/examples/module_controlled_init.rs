//! Native ABI fixture with bounded, externally released test barriers.
use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex};
use tinybus::module::abi::*;

#[repr(C)]
#[derive(Clone, Copy)]
struct Controls {
    ctx: *mut c_void,
    entered: unsafe extern "C" fn(*mut c_void, *const TbHostVtable),
    joined: unsafe extern "C" fn(*mut c_void),
}
unsafe impl Send for Controls {}

struct Instance {
    controls: Controls,
    host: TbHostVtable,
    stop: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[unsafe(no_mangle)]
pub static TINYBUS_MODULE_ABI_V1: TbAbiDescriptor =
    TbAbiDescriptor::current("controlled-init", env!("CARGO_PKG_VERSION"));

#[unsafe(no_mangle)]
pub extern "C" fn tinybus_module_manifest_v1() -> TbSlice {
    tinybus_module::manifest_slice(tinybus_module::ManifestDeclaration {
        name: "controlled-init",
        version: env!("CARGO_PKG_VERSION"),
        provides: &["ai.tinyhumans.ControlledInit"],
        methods: &[],
        signals: &[],
        requires: &[],
        optional: &[],
        lazy: true,
        worker_threads: 1,
    })
}

unsafe extern "C" fn deliver(_: *mut c_void, _: *const u8, _: usize) -> i32 {
    TB_OK
}
unsafe extern "C" fn reinitialize(ctx: *mut c_void, _: *const u8, _: usize) -> i32 {
    let instance = unsafe { &*ctx.cast::<Instance>() };
    unsafe {
        (instance.controls.entered)(instance.controls.ctx, &instance.host);
    }
    TB_OK
}
unsafe extern "C" fn shutdown(ctx: *mut c_void, _: u64) -> i32 {
    let mut instance = unsafe { Box::from_raw(ctx.cast::<Instance>()) };
    *instance.stop.0.lock().unwrap() = true;
    instance.stop.1.notify_all();
    instance.worker.take().unwrap().join().unwrap();
    unsafe {
        (instance.controls.joined)(instance.controls.ctx);
    }
    TB_OK
}

/// # Safety
/// Host pointers are valid for the ABI call; config contains test Controls.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tinybus_module_init_v1(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
) -> i32 {
    let host = unsafe { &*host };
    if host.config.len != size_of::<Controls>() {
        return TB_BAD_ARGUMENT;
    }
    let controls = unsafe { host.config.ptr.cast::<Controls>().read_unaligned() };
    let copied_config =
        unsafe { std::slice::from_raw_parts(host.config.ptr, host.config.len) }.to_vec();
    // The host-side barrier blocks actual opaque C entry, retaining its config
    // borrow through fault publication and initiating waiter abandonment.
    unsafe {
        (controls.entered)(controls.ctx, host);
    }
    assert_eq!(
        unsafe { std::slice::from_raw_parts(host.config.ptr, host.config.len) },
        copied_config
    );
    let stop = Arc::new((Mutex::new(false), Condvar::new()));
    let worker_stop = stop.clone();
    let worker = std::thread::spawn(move || {
        let mut stopped = worker_stop.0.lock().unwrap();
        while !*stopped {
            stopped = worker_stop.1.wait(stopped).unwrap();
        }
    });
    let instance = Box::new(Instance {
        controls,
        host: *host,
        stop,
        worker: Some(worker),
    });
    unsafe {
        *out = TbModuleVtable {
            size: size_of::<TbModuleVtable>() as u32,
            _reserved: 0,
            module_ctx: Box::into_raw(instance).cast(),
            deliver,
            shutdown,
            reinitialize: Some(reinitialize),
        };
    }
    TB_OK
}
