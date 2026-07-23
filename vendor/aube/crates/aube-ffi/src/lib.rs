//! C ABI surface for embedding aube through dynamic FFI loaders.

use aube::embed::{
    self, AddToProjectOptions, DepSelection, FrozenMode, InstallControl, InstallEvent,
    InstallOutputLevel, InstallPhase, InstallReporter, NetworkMode,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::{CStr, CString, c_char, c_void};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

pub type AubeEventCallback = unsafe extern "C" fn(*const c_char, *mut c_void);

const STATUS_OK: i32 = 0;
const STATUS_INVALID: i32 = -1;
const STATUS_PANIC: i32 = -2;

static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);
static JOBS: OnceLock<Mutex<HashMap<u64, Arc<Job>>>> = OnceLock::new();
static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
static HOST_INITIALIZED: OnceLock<()> = OnceLock::new();
static HOST_INIT_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug)]
struct Failure {
    code: String,
    message: String,
}

impl Failure {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(aube_codes::errors::ERR_AUBE_FFI_INVALID_ARGUMENT, message)
    }

    fn panic() -> Self {
        Self::new(
            aube_codes::errors::ERR_AUBE_FFI_PANIC,
            "a panic was caught at the aube C ABI boundary",
        )
    }
}

struct Job {
    control: InstallControl,
    callback: CallbackReporter,
    wait_claimed: AtomicBool,
    result: Mutex<Option<String>>,
    ready: Condvar,
}

impl Job {
    fn new(control: InstallControl, callback: CallbackReporter) -> Self {
        Self {
            control,
            callback,
            wait_claimed: AtomicBool::new(false),
            result: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    fn finish(&self, result: Result<(), Failure>) {
        if let Err(error) = &result {
            self.callback.report_payload(&EventPayload::Output {
                level: "error",
                code: Some(error.code.clone()),
                message: error.message.clone(),
            });
        }
        let json = result_json(result);
        let mut slot = self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(json);
        self.ready.notify_all();
    }

    fn wait(&self) -> String {
        let mut slot = self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while slot.is_none() {
            slot = self
                .ready
                .wait(slot)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        slot.clone().unwrap_or_else(|| {
            result_json(Err(Failure::new(
                aube_codes::errors::ERR_AUBE_FFI_RUNTIME,
                "operation completed without a result",
            )))
        })
    }

    fn claim_wait(&self) -> bool {
        self.wait_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// Bounded FIFO of serialized events for hosts that poll instead of
/// registering a callback (e.g. Bun, where cross-thread JS callbacks are
/// unsupported). When full, the oldest event is dropped; progress consumers
/// only care about the most recent snapshot.
struct EventBuffer {
    events: Mutex<VecDeque<String>>,
}

const EVENT_BUFFER_CAP: usize = 4096;

impl EventBuffer {
    fn new() -> Self {
        Self {
            events: Mutex::new(VecDeque::new()),
        }
    }

    fn push(&self, json: String) {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if events.len() == EVENT_BUFFER_CAP {
            events.pop_front();
        }
        events.push_back(json);
    }

    fn next(&self) -> Option<String> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
    }
}

#[derive(Clone)]
struct CallbackReporter {
    callback: Option<AubeEventCallback>,
    context: usize,
    buffer: Option<Arc<EventBuffer>>,
}

impl CallbackReporter {
    fn report_payload(&self, event: &EventPayload) {
        if self.callback.is_none() && self.buffer.is_none() {
            return;
        }
        let Ok(json) = serde_json::to_string(event) else {
            return;
        };
        if let Some(buffer) = &self.buffer {
            buffer.push(json.clone());
        }
        let Some(callback) = self.callback else {
            return;
        };
        let Ok(json) = CString::new(json) else {
            return;
        };
        // SAFETY: The host promises that callback and context remain valid and
        // thread-safe until aube_wait returns. The callback contract also
        // forbids waiting on this operation or unwinding across the boundary.
        // The CString lives through the call.
        unsafe { callback(json.as_ptr(), self.context as *mut c_void) };
    }
}

impl InstallReporter for CallbackReporter {
    fn report(&self, event: InstallEvent) {
        self.report_payload(&EventPayload::from(event));
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum EventPayload {
    Phase {
        phase: &'static str,
    },
    Progress {
        resolved: u64,
        total: u64,
        reused: u64,
        downloaded: u64,
        #[serde(rename = "downloadedBytes")]
        downloaded_bytes: u64,
        #[serde(rename = "estimatedBytes", skip_serializing_if = "Option::is_none")]
        estimated_bytes: Option<u64>,
    },
    Output {
        level: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        message: String,
    },
}

impl From<InstallEvent> for EventPayload {
    fn from(event: InstallEvent) -> Self {
        match event {
            InstallEvent::Phase(phase) => Self::Phase {
                phase: match phase {
                    InstallPhase::Resolving => "resolving",
                    InstallPhase::Fetching => "fetching",
                    InstallPhase::Linking => "linking",
                    InstallPhase::Complete => "complete",
                },
            },
            InstallEvent::Progress(progress) => Self::Progress {
                resolved: progress.resolved as u64,
                total: progress.total as u64,
                reused: progress.reused as u64,
                downloaded: progress.downloaded as u64,
                downloaded_bytes: progress.downloaded_bytes,
                estimated_bytes: (progress.estimated_bytes > 0).then_some(progress.estimated_bytes),
            },
            InstallEvent::Output {
                level,
                code,
                message,
            } => Self::Output {
                level: match level {
                    InstallOutputLevel::Info => "info",
                    InstallOutputLevel::Warning => "warning",
                    InstallOutputLevel::Error => "error",
                },
                code,
                message,
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HostInput {
    name: String,
    version: String,
    #[serde(default)]
    defaults: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstallInput {
    project_dir: PathBuf,
    #[serde(default)]
    frozen_mode: FrozenInput,
    #[serde(default)]
    prod_only: bool,
    #[serde(default)]
    dev_only: bool,
    #[serde(default)]
    omit_optional: bool,
    #[serde(default)]
    ignore_scripts: bool,
    #[serde(default = "default_true")]
    run_root_lifecycle: bool,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    lockfile_only: bool,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    offline: bool,
    #[serde(default)]
    strict_no_lockfile: bool,
    #[serde(default)]
    dangerously_allow_all_builds: bool,
    #[serde(default)]
    osv_transitive_check: bool,
    #[serde(default)]
    buffer_events: bool,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
enum FrozenInput {
    Frozen,
    #[default]
    Prefer,
    No,
    Fix,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct AddInput {
    save_dev: bool,
    save_exact: bool,
    save_optional: bool,
    save_peer: bool,
    ignore_scripts: bool,
    force: bool,
    dangerously_allow_all_builds: bool,
    offline: bool,
    prod_only: bool,
    dev_only: bool,
    omit_optional: bool,
    osv_transitive_check: bool,
    buffer_events: bool,
}

fn default_true() -> bool {
    true
}

/// Initialize the process-global embedding host.
#[unsafe(no_mangle)]
pub extern "C" fn aube_init(host_json: *const c_char) -> i32 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| init_impl(host_json))) {
        Ok(Ok(())) => STATUS_OK,
        Ok(Err(_)) => STATUS_INVALID,
        Err(_) => STATUS_PANIC,
    }
}

/// Start an asynchronous install and return its operation handle.
#[unsafe(no_mangle)]
pub extern "C" fn aube_install(
    options_json: *const c_char,
    callback: Option<AubeEventCallback>,
    context: *mut c_void,
) -> u64 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        install_impl(options_json, callback, context)
    })) {
        Ok(Ok(handle)) => handle,
        Ok(Err(error)) => completed_failure(error),
        Err(_) => completed_failure(Failure::panic()),
    }
}

/// Start an asynchronous package add and return its operation handle.
#[unsafe(no_mangle)]
pub extern "C" fn aube_add(
    project_dir: *const c_char,
    packages_json: *const c_char,
    options_json: *const c_char,
    callback: Option<AubeEventCallback>,
    context: *mut c_void,
) -> u64 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        add_impl(project_dir, packages_json, options_json, callback, context)
    })) {
        Ok(Ok(handle)) => handle,
        Ok(Err(error)) => completed_failure(error),
        Err(_) => completed_failure(Failure::panic()),
    }
}

/// Block until an operation completes and return owned result JSON.
#[unsafe(no_mangle)]
pub extern "C" fn aube_wait(handle: u64) -> *mut c_char {
    let json = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wait_impl(handle))) {
        Ok(result) => result,
        Err(_) => result_json(Err(Failure::panic())),
    };
    owned_c_string(json)
}

/// Return the next buffered event for an operation started with
/// `bufferEvents: true`, or null when no event is pending (or the handle is
/// unknown or already consumed). The returned string must be freed with
/// [`aube_string_free`]. Events still buffered when [`aube_wait`] returns are
/// discarded with the handle.
#[unsafe(no_mangle)]
pub extern "C" fn aube_events_next(handle: u64) -> *mut c_char {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| events_next_impl(handle))) {
        Ok(Some(json)) => owned_c_string(json),
        _ => std::ptr::null_mut(),
    }
}

/// Request cooperative cancellation for an operation.
#[unsafe(no_mangle)]
pub extern "C" fn aube_cancel(handle: u64) -> i32 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cancel_impl(handle))) {
        Ok(true) => STATUS_OK,
        Ok(false) => STATUS_INVALID,
        Err(_) => STATUS_PANIC,
    }
}

/// Free a string returned by this library.
///
/// # Safety
///
/// `value` must be null or an owned pointer returned by [`aube_wait`] that has
/// not previously been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aube_string_free(value: *mut c_char) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !value.is_null() {
            // SAFETY: The API contract requires a pointer returned by aube_wait
            // and permits exactly one call to aube_string_free for that pointer.
            unsafe { drop(CString::from_raw(value)) };
        }
    }));
}

fn init_impl(host_json: *const c_char) -> Result<(), Failure> {
    if HOST_INITIALIZED.get().is_some() {
        return Ok(());
    }
    let _guard = HOST_INIT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if HOST_INITIALIZED.get().is_some() {
        return Ok(());
    }

    let input: HostInput = parse_json(host_json, "host_json")?;
    if input.name.is_empty()
        || !input.name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(Failure::invalid(
            "host name must contain only lowercase ASCII letters, digits, '-' or '_'",
        ));
    }
    if input.version.is_empty() {
        return Err(Failure::invalid("host version must not be empty"));
    }
    let name = leak_string(input.name);
    let version = leak_string(input.version);
    let user_agent = leak_string(format!("{name}/{version}"));
    let self_names = Box::leak(vec![name].into_boxed_slice());
    let host = Box::leak(Box::new(embed::Host {
        name,
        display_name: name,
        vendor: None,
        version,
        user_agent,
        self_names,
        compatible_names: &["npm", "pnpm", "bun", "yarn"],
        lockfile_basename: "aube-lock.yaml",
        workspace_yaml: None,
        manifest_namespace: "",
        env_prefix: None,
        config_env_prefix: None,
        cache_namespace: name,
        data_namespace: name,
        canonical_lockfile_always_wins: false,
        runtime_switching: false,
        self_engines_check: false,
        self_update_enabled: false,
        // nub's fork adds embedder-fixed fields upstream's `Host` does not
        // have. Inheriting them from `AUBE` keeps this host on standalone-aube
        // behavior for every field upstream doesn't name, and survives either
        // side adding more.
        ..embed::AUBE
    }));
    embed::initialize(host, input.defaults.into_iter().collect());
    let _ = HOST_INITIALIZED.set(());
    Ok(())
}

fn seal_host_initialization() {
    if HOST_INITIALIZED.get().is_some() {
        return;
    }
    let _guard = HOST_INIT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if HOST_INITIALIZED.get().is_none() {
        embed::initialize(&embed::AUBE, Vec::new());
        let _ = HOST_INITIALIZED.set(());
    }
}

fn install_impl(
    options_json: *const c_char,
    callback: Option<AubeEventCallback>,
    context: *mut c_void,
) -> Result<u64, Failure> {
    let input: InstallInput = parse_json(options_json, "options_json")?;
    if input.prod_only && input.dev_only {
        return Err(Failure::invalid("prodOnly and devOnly cannot both be true"));
    }
    seal_host_initialization();
    let reporter = CallbackReporter {
        callback,
        context: context as usize,
        buffer: input.buffer_events.then(|| Arc::new(EventBuffer::new())),
    };
    let runtime = runtime()?;
    let control = InstallControl::events(Arc::new(reporter.clone()));
    let (handle, job) = register_job(control.clone(), reporter);
    spawn_job(runtime, job, async move {
        let mut options = embed::InstallOptions::new(input.project_dir);
        options.frozen_mode = match input.frozen_mode {
            FrozenInput::Frozen => FrozenMode::Frozen,
            FrozenInput::Prefer => FrozenMode::Prefer,
            FrozenInput::No => FrozenMode::No,
            FrozenInput::Fix => FrozenMode::Fix,
        };
        options.dep_selection =
            DepSelection::from_flags(input.prod_only, input.dev_only, input.omit_optional);
        options.ignore_scripts = input.ignore_scripts;
        options.run_root_lifecycle = input.run_root_lifecycle;
        options.dry_run = input.dry_run;
        options.lockfile_only = input.lockfile_only;
        options.force = input.force;
        options.network_mode = if input.offline {
            NetworkMode::Offline
        } else {
            NetworkMode::Online
        };
        options.strict_no_lockfile = input.strict_no_lockfile;
        options.dangerously_allow_all_builds = input.dangerously_allow_all_builds;
        options.osv_transitive_check = input.osv_transitive_check && !input.offline;
        options.control = control;
        embed::install(options).await.map_err(embed_failure)
    });
    Ok(handle)
}

fn add_impl(
    project_dir: *const c_char,
    packages_json: *const c_char,
    options_json: *const c_char,
    callback: Option<AubeEventCallback>,
    context: *mut c_void,
) -> Result<u64, Failure> {
    let project_dir = PathBuf::from(borrowed_string(project_dir, "project_dir")?);
    let packages: Vec<String> = parse_json(packages_json, "packages_json")?;
    let input: AddInput = parse_json(options_json, "options_json")?;
    if input.prod_only && input.dev_only {
        return Err(Failure::invalid("prodOnly and devOnly cannot both be true"));
    }
    seal_host_initialization();
    let reporter = CallbackReporter {
        callback,
        context: context as usize,
        buffer: input.buffer_events.then(|| Arc::new(EventBuffer::new())),
    };
    let runtime = runtime()?;
    let control = InstallControl::events(Arc::new(reporter.clone()));
    let (handle, job) = register_job(control.clone(), reporter);
    spawn_job(runtime, job, async move {
        let options = AddToProjectOptions {
            save_dev: input.save_dev,
            save_exact: input.save_exact,
            save_optional: input.save_optional,
            save_peer: input.save_peer,
            ignore_scripts: input.ignore_scripts,
            force: input.force,
            dangerously_allow_all_builds: input.dangerously_allow_all_builds,
            offline: input.offline,
            dep_selection: DepSelection::from_flags(
                input.prod_only,
                input.dev_only,
                input.omit_optional,
            ),
            osv_transitive_check: input.osv_transitive_check && !input.offline,
            control,
            // C ABI hosts rely on ambient node; no host-supplied runtime.
            node_bin_dir: None,
        };
        embed::add(&project_dir, &packages, options)
            .await
            .map_err(embed_failure)
    });
    Ok(handle)
}

fn register_job(control: InstallControl, callback: CallbackReporter) -> (u64, Arc<Job>) {
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    let job = Arc::new(Job::new(control, callback));
    jobs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(handle, Arc::clone(&job));
    (handle, job)
}

fn spawn_job<F>(runtime: &'static tokio::runtime::Runtime, job: Arc<Job>, operation: F)
where
    F: Future<Output = Result<(), Failure>> + Send + 'static,
{
    let worker_job = Arc::clone(&job);
    let worker = runtime.spawn(async move {
        worker_job.finish(operation.await);
    });
    runtime.spawn(async move {
        if let Err(error) = worker.await {
            let failure = if error.is_panic() {
                Failure::panic()
            } else {
                Failure::new(
                    aube_codes::errors::ERR_AUBE_FFI_RUNTIME,
                    format!("aube FFI operation task failed: {error}"),
                )
            };
            job.finish(Err(failure));
        }
    });
}

fn completed_failure(error: Failure) -> u64 {
    let reporter = CallbackReporter {
        callback: None,
        context: 0,
        buffer: None,
    };
    let (handle, job) = register_job(InstallControl::silent(), reporter);
    job.finish(Err(error));
    handle
}

fn wait_impl(handle: u64) -> String {
    let job = jobs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&handle)
        .cloned();
    let Some(job) = job else {
        return result_json(Err(Failure::new(
            aube_codes::errors::ERR_AUBE_FFI_UNKNOWN_HANDLE,
            format!("unknown or already-consumed operation handle {handle}"),
        )));
    };
    if !job.claim_wait() {
        return result_json(Err(Failure::new(
            aube_codes::errors::ERR_AUBE_FFI_UNKNOWN_HANDLE,
            format!("operation handle {handle} is already being consumed"),
        )));
    }
    let result = job.wait();
    jobs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&handle);
    result
}

fn events_next_impl(handle: u64) -> Option<String> {
    // Dequeue while holding the jobs lock so a poll cannot race aube_wait's
    // removal and return an event for an already-consumed handle.
    jobs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&handle)?
        .callback
        .buffer
        .as_ref()?
        .next()
}

fn cancel_impl(handle: u64) -> bool {
    let control = jobs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&handle)
        .map(|job| job.control.clone());
    if let Some(control) = control {
        control.cancel();
        true
    } else {
        false
    }
}

fn jobs() -> &'static Mutex<HashMap<u64, Arc<Job>>> {
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn runtime() -> Result<&'static tokio::runtime::Runtime, Failure> {
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("aube-ffi")
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|message| {
            Failure::new(
                aube_codes::errors::ERR_AUBE_FFI_RUNTIME,
                format!("failed to initialize the aube FFI runtime: {message}"),
            )
        })
}

fn embed_failure(error: miette::Report) -> Failure {
    Failure {
        code: embed::error_code(&error)
            .unwrap_or_else(|| aube_codes::errors::ERR_AUBE_EMBED_INSTALL_FAILED.to_string()),
        message: error.to_string(),
    }
}

fn result_json(result: Result<(), Failure>) -> String {
    #[derive(Serialize)]
    struct ResultPayload<'a> {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<&'a str>,
    }
    let payload = match &result {
        Ok(()) => ResultPayload {
            ok: true,
            code: None,
            message: None,
        },
        Err(error) => ResultPayload {
            ok: false,
            code: Some(&error.code),
            message: Some(&error.message),
        },
    };
    serde_json::to_string(&payload).unwrap_or_else(|_| {
        format!(
            r#"{{"ok":false,"code":"{}","message":"failed to serialize operation result"}}"#,
            aube_codes::errors::ERR_AUBE_FFI_RUNTIME
        )
    })
}

fn parse_json<T: for<'de> Deserialize<'de>>(
    value: *const c_char,
    label: &str,
) -> Result<T, Failure> {
    let value = borrowed_string(value, label)?;
    serde_json::from_str(&value)
        .map_err(|error| Failure::invalid(format!("invalid {label}: {error}")))
}

fn borrowed_string(value: *const c_char, label: &str) -> Result<String, Failure> {
    if value.is_null() {
        return Err(Failure::invalid(format!("{label} must not be null")));
    }
    // SAFETY: The API requires a NUL-terminated pointer valid for the duration
    // of this call. The returned owned String copies the input before return.
    let value = unsafe { CStr::from_ptr(value) };
    value
        .to_str()
        .map(str::to_owned)
        .map_err(|error| Failure::invalid(format!("{label} must be valid UTF-8: {error}")))
}

fn owned_c_string(value: String) -> *mut c_char {
    CString::new(value)
        .map(CString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

fn leak_string(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;
    use std::time::Duration;

    fn c(value: impl AsRef<str>) -> CString {
        CString::new(value.as_ref()).unwrap()
    }

    fn initialize() {
        let host = c(r#"{"name":"ffi-test","version":"1.0.0"}"#);
        assert_eq!(aube_init(host.as_ptr()), STATUS_OK);
    }

    fn wait(handle: u64) -> serde_json::Value {
        let value = aube_wait(handle);
        assert!(!value.is_null());
        // SAFETY: aube_wait returned an owned NUL-terminated string.
        let json = unsafe { CStr::from_ptr(value) }
            .to_str()
            .unwrap()
            .to_string();
        // SAFETY: value was returned by aube_wait and has not been freed.
        unsafe { aube_string_free(value) };
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn initialization_is_concurrent_and_idempotent() {
        std::thread::scope(|scope| {
            let threads = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        let host = c(r#"{"name":"ffi-test","version":"1.0.0"}"#);
                        aube_init(host.as_ptr())
                    })
                })
                .collect::<Vec<_>>();
            for thread in threads {
                assert_eq!(thread.join().unwrap(), STATUS_OK);
            }
        });
        assert_eq!(embed::host().name, "ffi-test");

        let malformed = c("{");
        assert_eq!(aube_init(malformed.as_ptr()), STATUS_OK);
        assert_eq!(aube_init(std::ptr::null()), STATUS_OK);
    }

    #[test]
    fn task_panics_complete_the_job() {
        initialize();
        let reporter = CallbackReporter {
            callback: None,
            context: 0,
            buffer: None,
        };
        let (handle, job) = register_job(InstallControl::silent(), reporter);
        spawn_job(runtime().unwrap(), job, async {
            panic!("simulated operation panic");
        });
        assert_eq!(wait(handle)["code"], aube_codes::errors::ERR_AUBE_FFI_PANIC);
    }

    #[test]
    fn concurrent_wait_has_a_single_consumer() {
        initialize();
        let reporter = CallbackReporter {
            callback: None,
            context: 0,
            buffer: None,
        };
        let (handle, job) = register_job(InstallControl::silent(), reporter);
        let (sender, receiver) = mpsc::channel();
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let sender = sender.clone();
                scope.spawn(move || sender.send(wait(handle)).unwrap());
            }
            let rejected = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(
                rejected["code"],
                aube_codes::errors::ERR_AUBE_FFI_UNKNOWN_HANDLE
            );
            job.finish(Ok(()));
            assert_eq!(
                receiver.recv_timeout(Duration::from_secs(1)).unwrap()["ok"],
                true
            );
        });
    }

    #[test]
    fn installs_offline_and_reports_events() {
        initialize();
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("package.json"), "{}\n").unwrap();
        let options = c(serde_json::json!({
            "projectDir": project.path(),
            "offline": true,
            "ignoreScripts": true
        })
        .to_string());
        let events = Mutex::new(Vec::<String>::new());
        unsafe extern "C" fn callback(event: *const c_char, context: *mut c_void) {
            // SAFETY: The test passes a live Mutex as context and the event
            // pointer is valid for the duration of this callback.
            let (events, event) = unsafe {
                (
                    &*(context.cast::<Mutex<Vec<String>>>()),
                    CStr::from_ptr(event).to_string_lossy().into_owned(),
                )
            };
            events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }
        let handle = aube_install(
            options.as_ptr(),
            Some(callback),
            (&events as *const Mutex<Vec<String>>).cast_mut().cast(),
        );
        assert_eq!(wait(handle)["ok"], true);
        assert!(
            events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|event| event.contains(r#""phase":"complete""#))
        );
    }

    #[test]
    fn returns_structured_boundary_and_project_errors() {
        initialize();
        let malformed = c("{");
        let malformed_result = wait(aube_install(malformed.as_ptr(), None, std::ptr::null_mut()));
        assert_eq!(
            malformed_result["code"],
            aube_codes::errors::ERR_AUBE_FFI_INVALID_ARGUMENT
        );

        let project = tempfile::tempdir().unwrap();
        let parent = project.path().join("file");
        fs::write(&parent, "not a directory").unwrap();
        let options = c(serde_json::json!({ "projectDir": parent.join("child") }).to_string());
        let project_result = wait(aube_install(options.as_ptr(), None, std::ptr::null_mut()));
        assert_eq!(
            project_result["code"],
            aube_codes::errors::ERR_AUBE_EMBED_INSTALL_FAILED
        );
    }

    fn next_event(handle: u64) -> Option<String> {
        let value = aube_events_next(handle);
        if value.is_null() {
            return None;
        }
        // SAFETY: aube_events_next returned an owned NUL-terminated string.
        let json = unsafe { CStr::from_ptr(value) }
            .to_str()
            .unwrap()
            .to_string();
        // SAFETY: value was returned by aube_events_next and has not been freed.
        unsafe { aube_string_free(value) };
        Some(json)
    }

    #[test]
    fn polls_buffered_events_without_a_callback() {
        initialize();
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("package.json"), "{}\n").unwrap();
        let options = c(serde_json::json!({
            "projectDir": project.path(),
            "offline": true,
            "ignoreScripts": true,
            "bufferEvents": true
        })
        .to_string());
        let handle = aube_install(options.as_ptr(), None, std::ptr::null_mut());

        let mut events = Vec::new();
        for _ in 0..600 {
            while let Some(event) = next_event(handle) {
                events.push(event);
            }
            if events
                .iter()
                .any(|event| event.contains(r#""phase":"complete""#))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            events
                .iter()
                .any(|event| event.contains(r#""phase":"complete""#))
        );
        assert_eq!(wait(handle)["ok"], true);
        assert!(aube_events_next(handle).is_null());
    }

    #[test]
    fn events_next_is_null_without_buffering_or_for_unknown_handles() {
        initialize();
        assert!(aube_events_next(u64::MAX).is_null());

        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("package.json"), "{}\n").unwrap();
        let options = c(serde_json::json!({
            "projectDir": project.path(),
            "offline": true,
            "ignoreScripts": true
        })
        .to_string());
        let handle = aube_install(options.as_ptr(), None, std::ptr::null_mut());
        assert!(aube_events_next(handle).is_null());
        assert_eq!(wait(handle)["ok"], true);
    }

    #[test]
    fn cancels_an_in_flight_add() {
        initialize();
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("package.json"), "{}\n").unwrap();
        fs::write(
            project.path().join(".npmrc"),
            "registry=http://10.255.255.1/\nfetch-timeout=60000\n",
        )
        .unwrap();
        let project_dir = c(project.path().to_string_lossy());
        let packages = c(r#"["aube-ffi-cancel-test"]"#);
        let options = c("{}");
        let handle = aube_add(
            project_dir.as_ptr(),
            packages.as_ptr(),
            options.as_ptr(),
            None,
            std::ptr::null_mut(),
        );
        assert_eq!(aube_cancel(handle), STATUS_OK);
        assert_eq!(
            wait(handle)["code"],
            aube_codes::errors::ERR_AUBE_INSTALL_CANCELLED
        );
    }
}
