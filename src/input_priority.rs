//! macOS scheduling for short, latency-sensitive input work. Clipboard, display
//! discovery and GUI status workers retain their normal scheduling class.
use std::{marker::PhantomData, rc::Rc};

unsafe extern "C" {
    fn pthread_self() -> usize;
    fn pthread_get_qos_class_np(thread: usize, class: *mut u32, relative: *mut i32) -> i32;
    fn pthread_set_qos_class_self_np(class: u32, relative: i32) -> i32;
}

const USER_INTERACTIVE: u32 = 0x21;
const DEFAULT: u32 = 0x15;

pub(crate) struct InputPriority {
    previous: Option<(u32, i32)>,
    // Restoration must run on the same native thread as the request.
    _thread_bound: PhantomData<Rc<()>>,
}

impl InputPriority {
    pub fn enter() -> Self {
        let (mut class, mut relative) = (0, 0);
        let read = unsafe { pthread_get_qos_class_np(pthread_self(), &mut class, &mut relative) };
        let previous = if read == 0 {
            let result = unsafe { pthread_set_qos_class_self_np(USER_INTERACTIVE, 0) };
            if result == 0 {
                // UNSPECIFIED cannot be set through this API. A normal thread
                // without an explicit class returns to DEFAULT after the session.
                Some((if class == 0 { DEFAULT } else { class }, relative))
            } else {
                log::warn!("cannot request interactive input scheduling: {result}");
                None
            }
        } else {
            log::warn!("cannot read input thread scheduling: {read}");
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
        if let Some((class, relative)) = self.previous {
            let result = unsafe { pthread_set_qos_class_self_np(class, relative) };
            if result != 0 {
                log::warn!("cannot restore input thread scheduling: {result}");
            }
        }
    }
}
