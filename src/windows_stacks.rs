//! Every thread's stack, written to the log when the agent is stuck, so the
//! lock it waits on can be found afterwards. Each thread is suspended only
//! while its stack is unwound into a fixed buffer: a suspended thread may hold
//! the heap lock, so nothing allocates until it runs again.
use std::{ffi::c_void, fmt::Write, mem, ptr};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HMODULE, INVALID_HANDLE_VALUE},
    System::{
        Diagnostics::{
            Debug::{
                CONTEXT, CONTEXT_FULL_AMD64, GetThreadContext, ReadProcessMemory,
                RtlLookupFunctionEntry, RtlVirtualUnwind, SYMBOL_INFO, SYMOPT_DEFERRED_LOADS,
                SYMOPT_FAIL_CRITICAL_ERRORS, SYMOPT_NO_PROMPTS, SYMOPT_UNDNAME, SymCleanup,
                SymFromAddr, SymInitializeW, SymSetOptions, UNW_FLAG_NHANDLER,
            },
            ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
        },
        LibraryLoader::{
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            GetModuleFileNameW, GetModuleHandleExW,
        },
        Threading::{
            GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, OpenThread, ResumeThread,
            SuspendThread, THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
        },
    },
};

const MAX_FRAMES: usize = 64;
const MAX_NAME: usize = 512;

/// Windows rejects a `CONTEXT` that is not 16-byte aligned.
#[repr(C, align(16))]
struct Aligned(CONTEXT);

/// The stack of every thread in this process except the calling one.
pub(crate) fn capture() -> String {
    let symbols = Symbols::load();
    let own = unsafe { GetCurrentThreadId() };
    let mut text = String::new();
    for id in threads().into_iter().filter(|&id| id != own) {
        let mut frames = [0u64; MAX_FRAMES];
        let count = walk(id, &mut frames);
        let _ = writeln!(text, "thread {id}:");
        for &address in &frames[..count] {
            let _ = writeln!(text, "    {}", symbols.describe(address));
        }
    }
    text
}

fn threads() -> Vec<u32> {
    let process = unsafe { GetCurrentProcessId() };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut entry = THREADENTRY32 {
        dwSize: mem::size_of::<THREADENTRY32>() as u32,
        ..unsafe { mem::zeroed() }
    };
    let mut ids = Vec::new();
    let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == process {
            ids.push(entry.th32ThreadID);
        }
        more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    ids
}

/// Unwind thread `id` into `frames`; returns how many were found.
fn walk(id: u32, frames: &mut [u64]) -> usize {
    let access = THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION;
    let thread = unsafe { OpenThread(access, 0, id) };
    if thread.is_null() {
        return 0;
    }
    let mut count = 0;
    if unsafe { SuspendThread(thread) } != u32::MAX {
        let mut context: Aligned = unsafe { mem::zeroed() };
        context.0.ContextFlags = CONTEXT_FULL_AMD64;
        if unsafe { GetThreadContext(thread, &mut context.0) } != 0 {
            count = unwind(&mut context.0, frames);
        }
        unsafe { ResumeThread(thread) };
    }
    unsafe { CloseHandle(thread) };
    count
}

/// Follow return addresses with the unwind data Windows keeps for every
/// function. The top of the stack is read with `ReadProcessMemory`, so a
/// damaged stack ends the walk rather than the process.
fn unwind(context: &mut CONTEXT, frames: &mut [u64]) -> usize {
    let mut count = 0;
    while count < frames.len() && context.Rip != 0 {
        frames[count] = context.Rip;
        count += 1;
        let stack = context.Rsp;
        let mut image = 0;
        let function = unsafe { RtlLookupFunctionEntry(context.Rip, &mut image, ptr::null_mut()) };
        if function.is_null() {
            // A leaf function: its return address is on top of the stack.
            let mut caller = 0u64;
            let mut read = 0;
            if unsafe {
                ReadProcessMemory(
                    GetCurrentProcess(),
                    context.Rsp as *const c_void,
                    (&raw mut caller).cast(),
                    mem::size_of::<u64>(),
                    &mut read,
                )
            } == 0
            {
                break;
            }
            context.Rip = caller;
            context.Rsp += 8;
        } else {
            let mut handler = ptr::null_mut();
            let mut frame = 0;
            unsafe {
                RtlVirtualUnwind(
                    UNW_FLAG_NHANDLER,
                    image,
                    context.Rip,
                    function,
                    context,
                    &mut handler,
                    &mut frame,
                    ptr::null_mut(),
                )
            };
        }
        // Each caller's frame lies further up the stack; anything else is
        // a damaged stack, not a deeper caller.
        if context.Rsp <= stack {
            break;
        }
    }
    count
}

/// Names for code addresses, from a program's symbol file found next to it
/// or in Seam's settings folder.
struct Symbols {
    loaded: bool,
}

impl Symbols {
    fn load() -> Self {
        let mut folders = Vec::new();
        if let Some(folder) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|folder| folder.to_path_buf()))
        {
            folders.push(folder.display().to_string());
        }
        if let Ok(folder) = crate::config::config_dir() {
            folders.push(folder.display().to_string());
        }
        let search: Vec<u16> = folders.join(";").encode_utf16().chain(Some(0)).collect();
        unsafe {
            SymSetOptions(
                SYMOPT_UNDNAME
                    | SYMOPT_DEFERRED_LOADS
                    | SYMOPT_FAIL_CRITICAL_ERRORS
                    | SYMOPT_NO_PROMPTS,
            )
        };
        Self {
            loaded: unsafe { SymInitializeW(GetCurrentProcess(), search.as_ptr(), 1) } != 0,
        }
    }

    /// The function's name when known, and always its module and offset,
    /// which a symbol file can resolve later.
    fn describe(&self, address: u64) -> String {
        let (module, base) = module(address);
        let place = format!("{module}+{:#x}", address.wrapping_sub(base));
        match self.name(address) {
            Some(name) => format!("{name} ({place})"),
            None => place,
        }
    }

    fn name(&self, address: u64) -> Option<String> {
        if !self.loaded {
            return None;
        }
        // SYMBOL_INFO ends in a name buffer that the caller extends.
        let mut buffer = [0u64; (mem::size_of::<SYMBOL_INFO>() + MAX_NAME) / 8 + 1];
        let symbol = buffer.as_mut_ptr().cast::<SYMBOL_INFO>();
        unsafe {
            (*symbol).SizeOfStruct = mem::size_of::<SYMBOL_INFO>() as u32;
            (*symbol).MaxNameLen = MAX_NAME as u32;
        }
        let mut displacement = 0;
        if unsafe { SymFromAddr(GetCurrentProcess(), address, &mut displacement, symbol) } == 0 {
            return None;
        }
        let name = unsafe {
            std::slice::from_raw_parts(
                (*symbol).Name.as_ptr().cast::<u8>(),
                ((*symbol).NameLen as usize).min(MAX_NAME),
            )
        };
        Some(String::from_utf8_lossy(name).into_owned())
    }
}

impl Drop for Symbols {
    fn drop(&mut self) {
        if self.loaded {
            unsafe { SymCleanup(GetCurrentProcess()) };
        }
    }
}

/// The file name of the module containing `address`, and its base address.
fn module(address: u64) -> (String, u64) {
    let mut handle: HMODULE = ptr::null_mut();
    let flags =
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
    if unsafe { GetModuleHandleExW(flags, address as *const u16, &mut handle) } == 0 {
        return ("?".into(), 0);
    }
    let mut path = [0u16; 260];
    let length = unsafe { GetModuleFileNameW(handle, path.as_mut_ptr(), path.len() as u32) };
    let path = String::from_utf16_lossy(&path[..length as usize]);
    let name = path.rsplit('\\').next().unwrap_or_default().to_string();
    (name, handle as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, Mutex};

    #[test]
    fn unwinds_a_thread_waiting_on_a_lock_to_its_start() {
        let lock = Arc::new(Mutex::new(()));
        let ready = Arc::new(Barrier::new(2));
        let held = lock.lock().unwrap();
        let waiter = {
            let (lock, ready) = (lock.clone(), ready.clone());
            std::thread::spawn(move || {
                let id = unsafe { GetCurrentThreadId() };
                ready.wait();
                drop(lock.lock().unwrap());
                id
            })
        };
        ready.wait();
        // Long enough for the waiter to be blocked inside the lock.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let stacks = capture();
        drop(held);
        let waiter = waiter.join().unwrap();
        // Names that survive optimization: the wait inside Windows, and the
        // start of every thread, which only a complete walk reaches.
        let stack = stacks
            .split("thread ")
            .find(|section| section.starts_with(&format!("{waiter}:")))
            .unwrap_or_else(|| panic!("no stack for the waiting thread:\n{stacks}"));
        assert!(
            stack.contains("WaitOnAddress") && stack.contains("RtlUserThreadStart"),
            "the waiting thread's stack shows the wait and reaches its start:\n{stack}"
        );
        let own = unsafe { GetCurrentThreadId() };
        assert!(!stacks.contains(&format!("thread {own}:")));
    }
}
