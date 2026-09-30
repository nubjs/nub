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

use std::ffi::c_int;
use std::ffi::c_void;
use std::sync::OnceLock;

use napi_derive::napi;

const UV_WORK: c_int = 7;
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

struct Uv {
    loop_size: UvLoopSize,
    loop_init: UvLoopInit,
    loop_close: UvLoopClose,
    req_size: UvReqSize,
    queue_work: UvQueueWork,
    run: UvRun,
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
    unsafe {
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
