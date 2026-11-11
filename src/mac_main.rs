//! A Mac delivers another app's request for clipboard data that Seam only
//! promised, such as offered files that a paste fetches, on the main thread's
//! run loop. The agent therefore runs on a thread of its own while the main
//! thread runs that loop.
use std::{
    ffi::c_void,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRunLoopGetMain() -> *mut c_void;
    fn CFRunLoopRun();
    fn CFRunLoopStop(run_loop: *mut c_void);
    fn CFRunLoopWakeUp(run_loop: *mut c_void);
    fn CFRunLoopAddSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRemoveSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopSourceCreate(
        allocator: *const c_void,
        order: isize,
        context: *mut SourceContext,
    ) -> *mut c_void;
    fn CFRunLoopSourceSignal(source: *mut c_void);
    fn CFRunLoopSourceInvalidate(source: *mut c_void);
    fn CFRelease(object: *const c_void);
    static kCFRunLoopCommonModes: *const c_void;
}

// CFRunLoopSourceContext version 0. Its source keeps the main run loop
// running, and signalling it stops the loop once the agent has ended.
#[repr(C)]
struct SourceContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
    hash: *const c_void,
    schedule: *const c_void,
    cancel: *const c_void,
    perform: unsafe extern "C" fn(*mut c_void),
}

unsafe extern "C" fn stop_main_loop(_: *mut c_void) {
    unsafe { CFRunLoopStop(CFRunLoopGetMain()) };
}

/// Stops the main run loop when the agent's thread ends, even by a panic.
struct Finished {
    done: Arc<AtomicBool>,
    source: usize,
}

impl Drop for Finished {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        unsafe {
            CFRunLoopSourceSignal(self.source as *mut c_void);
            CFRunLoopWakeUp(CFRunLoopGetMain());
        }
    }
}

/// Runs `work` on its own thread while this, the main thread, runs the main
/// run loop, and returns what `work` returned.
pub(crate) fn beside_main_loop<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> anyhow::Result<T> {
    if objc2::MainThreadMarker::new().is_none() {
        return Ok(work());
    }
    let main = unsafe { CFRunLoopGetMain() };
    let mut context = SourceContext {
        version: 0,
        info: ptr::null_mut(),
        retain: ptr::null(),
        release: ptr::null(),
        copy_description: ptr::null(),
        equal: ptr::null(),
        hash: ptr::null(),
        schedule: ptr::null(),
        cancel: ptr::null(),
        perform: stop_main_loop,
    };
    let source = unsafe { CFRunLoopSourceCreate(ptr::null(), 0, &mut context) };
    anyhow::ensure!(
        !source.is_null(),
        "cannot create the main run loop's source"
    );
    unsafe { CFRunLoopAddSource(main, source, kCFRunLoopCommonModes) };
    let done = Arc::new(AtomicBool::new(false));
    let finished = Finished {
        done: done.clone(),
        source: source as usize,
    };
    let worker = std::thread::Builder::new()
        .name("agent".into())
        // As much stack as the main thread it used to run on.
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let _finished = finished;
            work()
        });
    if worker.is_ok() {
        // A signal sent before the loop starts still stops it.
        while !done.load(Ordering::Acquire) {
            unsafe { CFRunLoopRun() };
        }
    }
    unsafe {
        CFRunLoopRemoveSource(main, source, kCFRunLoopCommonModes);
        CFRunLoopSourceInvalidate(source);
        CFRelease(source);
    }
    match worker?.join() {
        Ok(result) => Ok(result),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
