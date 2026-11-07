//! Elevate only the short capture and encrypted input workers, not clipboard or
//! GUI work. Restore the old priority when a connection thread leaves its scope.
use std::{marker::PhantomData, rc::Rc};
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, GetThreadPriority, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};
pub(crate) struct InputPriority {
    previous: Option<i32>,
    _thread_bound: PhantomData<Rc<()>>,
}
impl InputPriority {
    pub fn enter() -> Self {
        let thread = unsafe { GetCurrentThread() };
        let old = unsafe { GetThreadPriority(thread) };
        let previous = if old != i32::MAX
            && unsafe { SetThreadPriority(thread, old.max(THREAD_PRIORITY_ABOVE_NORMAL)) } != 0
        {
            Some(old)
        } else {
            log::warn!(
                "Cannot request responsive Windows input scheduling: {}",
                std::io::Error::last_os_error()
            );
            None
        };
        Self {
            previous,
            _thread_bound: PhantomData,
        }
    }
}
impl Drop for InputPriority {
    fn drop(&mut self) {
        if let Some(old) = self.previous {
            unsafe {
                SetThreadPriority(GetCurrentThread(), old);
            }
        }
    }
}
