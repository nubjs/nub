//! Build libuv's threadpool without leaving anything for user code to observe.
//!
//! libuv creates every worker synchronously inside the first pool submit, from
//! whichever loop submits, and the pool is process-wide. The preload needs that
//! to happen before it deletes nub's `UV_THREADPOOL_SIZE` from the environment
//! (libuv reads the variable lazily, at that first submit) and, on Linux, across
//! a call it brackets with a thread-id snapshot so the workers can be demoted.
//!
//! A JS submit (`fs.access`) is an `AsyncWrap` request on Node's main loop: its
//! completion callback runs after user code has started, so `async_hooks` sees a
//! `before` for an id it never saw an `init` for, `process.getActiveResourcesInfo()`
//! lists an `FSReqCallback`, and the loop is alive for one extra turn, which runs
//! an unref'd `setImmediate` plain Node would not. Node's own test suite asserts
//! on each of those. A raw `uv_queue_work` on a private loop, run to completion
//! here, touches none of Node's bookkeeping: no AsyncWrap, no main-loop request,
//! nothing left pending. The symbols are looked up in the host process at call
//! time, as napi-sys does for its own, so no platform needs an import library.
//!
//! The preload builds the pool at the event loop's first turn rather than before
//! user code, so a program that sets `UV_THREADPOOL_SIZE` itself before its first
//! pool use is honoured as under plain Node. Every JS way to run later is visible
//! the same way a JS submit is (a timer or an immediate is an async resource with
//! an `init` user hooks never saw), so [`on_first_loop_turn`] is a raw unref'd
//! `uv_prepare` on Node's loop instead: no AsyncWrap, never alive on its own.

use std::cell::Cell;
use std::cell::RefCell;
use std::ffi::c_int;
use std::ffi::c_void;
use std::ptr;
use std::sync::OnceLock;

use napi::Env;
use napi::JsValue;
use napi::bindgen_prelude::Function;
use napi::sys;
use napi_derive::napi;

const UV_WORK: c_int = 7;
const UV_PREPARE: c_int = 9;
const UV_RUN_DEFAULT: c_int = 0;

type UvLoopSize = unsafe extern "C" fn() -> usize;
type UvLoopInit = unsafe extern "C" fn(*mut c_void) -> c_int;
type UvLoopClose = unsafe extern "C" fn(*mut c_void) -> c_int;
type UvReqSize = unsafe extern "C" fn(c_int) -> usize;
type UvQueueWork = unsafe extern "C" fn(
    *mut c_void,
    *mut c_void,
    unsafe extern "C" fn(*mut c_void),
    unsafe extern "C" fn(*mut c_void, c_int),
) -> c_int;
type UvRun = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type UvHandleSize = unsafe extern "C" fn(c_int) -> usize;
type UvPrepareInit = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;
type UvPrepareStart = unsafe extern "C" fn(*mut c_void, unsafe extern "C" fn(*mut c_void)) -> c_int;
type UvPrepareStop = unsafe extern "C" fn(*mut c_void) -> c_int;
type UvUnref = unsafe extern "C" fn(*mut c_void);
type UvClose = unsafe extern "C" fn(*mut c_void, unsafe extern "C" fn(*mut c_void));

struct Uv {
    loop_size: UvLoopSize,
    loop_init: UvLoopInit,
    loop_close: UvLoopClose,
    req_size: UvReqSize,
    queue_work: UvQueueWork,
    run: UvRun,
    handle_size: UvHandleSize,
    prepare_init: UvPrepareInit,
    prepare_start: UvPrepareStart,
    prepare_stop: UvPrepareStop,
    unref: UvUnref,
    close: UvClose,
}

static UV: OnceLock<Option<Uv>> = OnceLock::new();

/// The host's libuv, or `None` when the process exports no libuv (an embedder
/// that hides it); the caller then falls back to a JS submit.
fn uv() -> Option<&'static Uv> {
    UV.get_or_init(|| unsafe {
        let host = host_library()?;
        macro_rules! sym {
            ($name:literal) => {
                *host.get::<unsafe extern "C" fn()>($name).ok()?
            };
        }
        // Each `sym!` yields a bare `fn()`; the transmutes restore the real
        // signatures, which are libuv's public C ABI.
        Some(Uv {
            loop_size: std::mem::transmute::<unsafe extern "C" fn(), UvLoopSize>(sym!(
                b"uv_loop_size\0"
            )),
            loop_init: std::mem::transmute::<unsafe extern "C" fn(), UvLoopInit>(sym!(
                b"uv_loop_init\0"
            )),
            loop_close: std::mem::transmute::<unsafe extern "C" fn(), UvLoopClose>(sym!(
                b"uv_loop_close\0"
            )),
            req_size: std::mem::transmute::<unsafe extern "C" fn(), UvReqSize>(sym!(
                b"uv_req_size\0"
            )),
            queue_work: std::mem::transmute::<unsafe extern "C" fn(), UvQueueWork>(sym!(
                b"uv_queue_work\0"
            )),
            run: std::mem::transmute::<unsafe extern "C" fn(), UvRun>(sym!(b"uv_run\0")),
            handle_size: std::mem::transmute::<unsafe extern "C" fn(), UvHandleSize>(sym!(
                b"uv_handle_size\0"
            )),
            prepare_init: std::mem::transmute::<unsafe extern "C" fn(), UvPrepareInit>(sym!(
                b"uv_prepare_init\0"
            )),
            prepare_start: std::mem::transmute::<unsafe extern "C" fn(), UvPrepareStart>(sym!(
                b"uv_prepare_start\0"
            )),
            prepare_stop: std::mem::transmute::<unsafe extern "C" fn(), UvPrepareStop>(sym!(
                b"uv_prepare_stop\0"
            )),
            unref: std::mem::transmute::<unsafe extern "C" fn(), UvUnref>(sym!(b"uv_unref\0")),
            close: std::mem::transmute::<unsafe extern "C" fn(), UvClose>(sym!(b"uv_close\0")),
        })
    })
    .as_ref()
}

/// The image that exports libuv: the executable itself for `node`, else the
/// shared `libnode` an embedder loaded (napi-sys probes the same two).
unsafe fn host_library() -> Option<libloading::Library> {
    #[cfg(windows)]
    unsafe {
        use libloading::os::windows::Library;
        let has_uv = |lib: Library| {
            lib.get::<unsafe extern "C" fn()>(b"uv_loop_size\0")
                .is_ok()
                .then_some(lib)
        };
        Library::this()
            .ok()
            .and_then(has_uv)
            .or_else(|| {
                Library::open_already_loaded("libnode.dll")
                    .ok()
                    .and_then(has_uv)
            })
            .map(Into::into)
    }
    #[cfg(not(windows))]
    {
        Some(libloading::os::unix::Library::this().into())
    }
}

unsafe extern "C" fn work(_req: *mut c_void) {}
unsafe extern "C" fn after_work(_req: *mut c_void, _status: c_int) {}

/// Submit one no-op task to libuv's threadpool on a private loop and wait for it,
/// so the pool exists (sized from the environment at that moment) with no request
/// or callback left behind on Node's loop. `false` when the host exports no libuv;
/// the JS caller then submits through `fs` instead.
#[napi]
pub fn warm_threadpool() -> bool {
    let Some(uv) = uv() else {
        return false;
    };
    unsafe {
        // libuv's structs hold pointers and 64-bit fields; a word-typed buffer
        // is aligned for them whatever the allocator does for bytes.
        let words = |bytes: usize| vec![0usize; bytes.div_ceil(size_of::<usize>())];
        let mut loop_buf = words((uv.loop_size)());
        let mut req_buf = words((uv.req_size)(UV_WORK));
        let loop_ptr = loop_buf.as_mut_ptr().cast::<c_void>();
        if (uv.loop_init)(loop_ptr) != 0 {
            return false;
        }
        let queued = (uv.queue_work)(
            loop_ptr,
            req_buf.as_mut_ptr().cast::<c_void>(),
            work,
            after_work,
        ) == 0;
        if queued {
            // Runs until the work's completion has been delivered, which
            // deactivates the request and leaves the loop with nothing alive.
            (uv.run)(loop_ptr, UV_RUN_DEFAULT);
        }
        (uv.loop_close)(loop_ptr);
        queued
    }
}

thread_local! {
    /// The armed callback's env and the strong reference that keeps it alive.
    static FIRST_TURN: Cell<Option<(sys::napi_env, sys::napi_ref)>> = const { Cell::new(None) };
    /// The prepare handle's memory, owned until libuv's close callback.
    static FIRST_TURN_HANDLE: RefCell<Option<Box<[usize]>>> = const { RefCell::new(None) };
}

/// Call `callback` once, at the start of Node's next event-loop turn: before that
/// turn polls, so a server idling on its sockets still reaches it, and after the
/// entry's synchronous evaluation and its microtasks. The handle is unref'd, so it
/// never keeps the process alive (a script whose loop never turns never calls
/// back), and it is closed inside the turn that runs it, so no extra turn follows.
/// `false` when the host exports no libuv or a callback is already armed; the
/// caller then does the work at once.
#[napi]
pub fn on_first_loop_turn(env: Env, callback: Function<'_>) -> bool {
    let Some(uv) = uv() else {
        return false;
    };
    if FIRST_TURN_HANDLE.with(|h| h.borrow().is_some()) {
        return false;
    }
    let raw_env = env.raw();
    unsafe {
        let mut loop_ptr: *mut sys::uv_loop_s = ptr::null_mut();
        if sys::napi_get_uv_event_loop(raw_env, &mut loop_ptr) != sys::Status::napi_ok
            || loop_ptr.is_null()
        {
            return false;
        }
        let mut handle = vec![0usize; (uv.handle_size)(UV_PREPARE).div_ceil(size_of::<usize>())]
            .into_boxed_slice();
        let handle_ptr = handle.as_mut_ptr().cast::<c_void>();
        if (uv.prepare_init)(loop_ptr.cast(), handle_ptr) != 0 {
            return false;
        }
        // Registered with the loop from here on, so the memory is libuv's until
        // the close callback hands it back.
        FIRST_TURN_HANDLE.with(|h| *h.borrow_mut() = Some(handle));
        let mut reference = ptr::null_mut();
        if sys::napi_create_reference(raw_env, callback.value().value, 1, &mut reference)
            != sys::Status::napi_ok
            || (uv.prepare_start)(handle_ptr, first_turn) != 0
        {
            if !reference.is_null() {
                sys::napi_delete_reference(raw_env, reference);
            }
            (uv.close)(handle_ptr, first_turn_closed);
            return false;
        }
        (uv.unref)(handle_ptr);
        FIRST_TURN.with(|c| c.set(Some((raw_env, reference))));
    }
    true
}

unsafe extern "C" fn first_turn(handle: *mut c_void) {
    let Some(uv) = uv() else {
        return;
    };
    unsafe {
        (uv.prepare_stop)(handle);
        (uv.close)(handle, first_turn_closed);
    }
    if let Some((env, reference)) = FIRST_TURN.with(Cell::take) {
        unsafe { call_and_release(env, reference) };
    }
}

unsafe extern "C" fn first_turn_closed(_handle: *mut c_void) {
    FIRST_TURN_HANDLE.with(|h| h.borrow_mut().take());
}

/// Call the referenced function with no arguments, then drop the reference. There
/// is no JS frame above a loop callback to receive an exception, so one is cleared
/// here rather than left pending on the env.
unsafe fn call_and_release(env: sys::napi_env, reference: sys::napi_ref) {
    unsafe {
        let mut scope = ptr::null_mut();
        if sys::napi_open_handle_scope(env, &mut scope) != sys::Status::napi_ok {
            return;
        }
        let mut function = ptr::null_mut();
        let mut receiver = ptr::null_mut();
        let mut result = ptr::null_mut();
        if sys::napi_get_reference_value(env, reference, &mut function) == sys::Status::napi_ok
            && sys::napi_get_undefined(env, &mut receiver) == sys::Status::napi_ok
            && sys::napi_call_function(env, receiver, function, 0, ptr::null(), &mut result)
                != sys::Status::napi_ok
        {
            let mut exception = ptr::null_mut();
            sys::napi_get_and_clear_last_exception(env, &mut exception);
        }
        sys::napi_delete_reference(env, reference);
        sys::napi_close_handle_scope(env, scope);
    }
}
