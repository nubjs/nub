//! Purpose-built Linux retained-monitor integration harness.

use std::process::ExitCode;

#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[cfg(target_os = "linux")]
static TARGET_SIGNAL_COUNT: AtomicUsize = AtomicUsize::new(0);
#[cfg(target_os = "linux")]
static TARGET_FINISH: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "linux")]
static TARGET_FINISH_REQUESTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "linux")]
static TARGET_REQUIRES_COUNT: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    let verb = std::env::args().nth(1);
    match verb.as_deref() {
        Some("target-exec-probe") => return target_exec_probe("target-exec-probe"),
        Some("target-exec-descendants") => return target_exec_descendants(),
        Some("target-exec-exit") => {
            if !target_exec_context_is_exact("target-exec-exit") {
                return ExitCode::from(124);
            }
            unsafe { libc::_exit(23) }
        }
        Some("target-exec-signal") => {
            if !target_exec_context_is_exact("target-exec-signal") {
                return ExitCode::from(124);
            }
            unsafe {
                libc::raise(libc::SIGTERM);
                libc::_exit(123)
            }
        }
        Some("target-exec-stop") => {
            if !target_exec_context_is_exact("target-exec-stop") {
                return ExitCode::from(124);
            }
            unsafe {
                libc::raise(libc::SIGSTOP);
                park_raw()
            }
        }
        Some("target-runtime-park") => return target_runtime_park(),
        Some("target-runtime-exit-143") => return target_runtime_exit_143(),
        Some("target-runtime-count-int") => return target_runtime_count(libc::SIGINT),
        Some("target-runtime-count-term") => return target_runtime_count(libc::SIGTERM),
        Some("state-8-outer-zero") => return ExitCode::SUCCESS,
        Some("state-8-outer-late-zero") => {
            std::thread::sleep(std::time::Duration::from_millis(300));
            return ExitCode::SUCCESS;
        }
        Some("state-8-outer-23") => return ExitCode::from(23),
        Some("state-8-outer-term") => unsafe {
            libc::raise(libc::SIGTERM);
            libc::_exit(124)
        },
        Some("state-8-outer-park") => unsafe { park_raw() },
        _ => {}
    }

    let runtime = match nub_sandbox::earliest_bootstrap() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("sandbox monitor harness bootstrap failed: {error}");
            return ExitCode::from(125);
        }
    };

    match verb.as_deref() {
        Some("identity") => {
            println!("nub-sandbox-monitor-harness");
            ExitCode::SUCCESS
        }
        Some("startup-probe") => ExitCode::SUCCESS,
        Some("verify-self-description") => {
            let result = std::env::current_exe()
                .and_then(nub_sandbox::RuntimeCapability::from_verified_executable);
            match result {
                Ok(_) => {
                    println!("verified-runtime-description");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("sandbox monitor runtime verification failed: {error}");
                    ExitCode::from(125)
                }
            }
        }
        Some("exercise-states-1-5") => {
            let bwrap = std::env::args_os()
                .nth(2)
                .unwrap_or_else(|| "/usr/bin/bwrap".into());
            match nub_sandbox::exercise_monitor_states_1_to_5(&runtime, bwrap) {
                Ok(()) => {
                    println!("verified-monitor-states-1-5");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("sandbox monitor states 1-5 failed: {error}");
                    ExitCode::from(125)
                }
            }
        }
        Some("exercise-state-6") => {
            let bwrap = std::env::args_os()
                .nth(2)
                .unwrap_or_else(|| "/usr/bin/bwrap".into());
            match nub_sandbox::exercise_monitor_state_6(&runtime, bwrap) {
                Ok(()) => {
                    println!("verified-monitor-state-6");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("sandbox monitor state 6 failed: {error}");
                    ExitCode::from(125)
                }
            }
        }
        Some("exercise-state-7") => {
            let bwrap = std::env::args_os()
                .nth(2)
                .unwrap_or_else(|| "/usr/bin/bwrap".into());
            match nub_sandbox::exercise_monitor_state_7(&runtime, bwrap) {
                Ok(()) => {
                    println!("verified-monitor-state-7");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("sandbox monitor state 7 failed: {error}");
                    ExitCode::from(125)
                }
            }
        }
        Some("exercise-state-8") => {
            let bwrap = std::env::args_os()
                .nth(2)
                .unwrap_or_else(|| "/usr/bin/bwrap".into());
            match nub_sandbox::exercise_monitor_state_8(&runtime, bwrap) {
                Ok(()) => {
                    println!("verified-monitor-state-8");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("sandbox monitor state 8 failed: {error}");
                    ExitCode::from(125)
                }
            }
        }
        _ => ExitCode::from(2),
    }
}

#[cfg(target_os = "linux")]
fn target_exec_probe(verb: &str) -> ExitCode {
    if !target_exec_context_is_exact(verb) {
        return ExitCode::from(124);
    }
    loop {
        std::thread::park();
    }
}

#[cfg(target_os = "linux")]
fn target_exec_descendants() -> ExitCode {
    if !target_exec_context_is_exact("target-exec-descendants") {
        return ExitCode::from(124);
    }
    let mut ready = [-1; 2];
    if unsafe { libc::pipe2(ready.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return ExitCode::from(123);
    }
    let child = unsafe { libc::fork() };
    if child < 0 {
        unsafe {
            libc::close(ready[0]);
            libc::close(ready[1]);
        }
        return ExitCode::from(123);
    }
    if child == 0 {
        unsafe {
            libc::close(ready[0]);
            if libc::setsid() < 0 {
                libc::_exit(122);
            }
            let grandchild = libc::fork();
            if grandchild < 0 {
                libc::_exit(121);
            }
            if grandchild == 0 {
                libc::close(ready[1]);
                park_raw();
            }
            let marker = [1_u8];
            if libc::write(ready[1], marker.as_ptr().cast(), marker.len()) != 1 {
                libc::_exit(119);
            }
            libc::close(ready[1]);
            park_raw();
        }
    }
    unsafe {
        libc::close(ready[1]);
    }
    let mut marker = [0_u8];
    let observed = unsafe { libc::read(ready[0], marker.as_mut_ptr().cast(), marker.len()) };
    unsafe {
        libc::close(ready[0]);
    }
    if observed != 1 || marker != [1] || !mark_runtime_ready() {
        return ExitCode::from(120);
    }
    loop {
        std::thread::park();
    }
}

#[cfg(target_os = "linux")]
fn target_runtime_park() -> ExitCode {
    if !target_exec_context_is_exact("target-runtime-park") || !mark_runtime_ready() {
        return ExitCode::from(124);
    }
    unsafe { park_raw() }
}

#[cfg(target_os = "linux")]
fn target_runtime_exit_143() -> ExitCode {
    if !target_exec_context_is_exact("target-runtime-exit-143") {
        return ExitCode::from(124);
    }
    TARGET_FINISH.store(false, Ordering::Relaxed);
    TARGET_FINISH_REQUESTED.store(false, Ordering::Relaxed);
    TARGET_REQUIRES_COUNT.store(false, Ordering::Relaxed);
    let previous = match prepare_runtime_signals(None) {
        Some(previous) => previous,
        None => return ExitCode::from(123),
    };
    if !mark_runtime_ready() {
        return ExitCode::from(122);
    }
    wait_for_runtime_finish(previous);
    unsafe { libc::_exit(143) }
}

#[cfg(target_os = "linux")]
fn target_runtime_count(signal: libc::c_int) -> ExitCode {
    let verb = if signal == libc::SIGINT {
        "target-runtime-count-int"
    } else {
        "target-runtime-count-term"
    };
    if !target_exec_context_is_exact(verb) {
        return ExitCode::from(124);
    }
    TARGET_SIGNAL_COUNT.store(0, Ordering::Relaxed);
    TARGET_FINISH.store(false, Ordering::Relaxed);
    TARGET_FINISH_REQUESTED.store(false, Ordering::Relaxed);
    TARGET_REQUIRES_COUNT.store(true, Ordering::Relaxed);
    let previous = match prepare_runtime_signals(Some(signal)) {
        Some(previous) => previous,
        None => return ExitCode::from(123),
    };
    if !mark_runtime_ready() {
        return ExitCode::from(122);
    }
    wait_for_runtime_finish(previous);
    let count = TARGET_SIGNAL_COUNT.load(Ordering::Relaxed).min(10) as u8;
    unsafe { libc::_exit(40 + count as libc::c_int) }
}

#[cfg(target_os = "linux")]
fn prepare_runtime_signals(counted: Option<libc::c_int>) -> Option<libc::sigset_t> {
    unsafe {
        let mut blocked = std::mem::zeroed::<libc::sigset_t>();
        let mut previous = std::mem::zeroed::<libc::sigset_t>();
        if libc::sigemptyset(&mut blocked) != 0
            || libc::sigaddset(&mut blocked, libc::SIGUSR1) != 0
            || counted.is_some_and(|signal| libc::sigaddset(&mut blocked, signal) != 0)
            || libc::sigprocmask(libc::SIG_BLOCK, &blocked, &mut previous) != 0
        {
            return None;
        }
        if install_signal_handler(libc::SIGUSR1, finish_signal_handler).is_err()
            || counted
                .is_some_and(|signal| install_signal_handler(signal, count_signal_handler).is_err())
        {
            return None;
        }
        Some(previous)
    }
}

#[cfg(target_os = "linux")]
unsafe fn install_signal_handler(
    signal: libc::c_int,
    handler: extern "C" fn(libc::c_int),
) -> Result<(), ()> {
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = handler as usize;
    action.sa_flags = 0;
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0
        || unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0
    {
        return Err(());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn wait_for_runtime_finish(mut previous: libc::sigset_t) {
    unsafe {
        libc::sigdelset(&mut previous, libc::SIGUSR1);
        libc::sigdelset(&mut previous, libc::SIGINT);
        libc::sigdelset(&mut previous, libc::SIGTERM);
        while !TARGET_FINISH.load(Ordering::Relaxed) {
            libc::sigsuspend(&previous);
        }
    }
}

#[cfg(target_os = "linux")]
extern "C" fn count_signal_handler(_signal: libc::c_int) {
    TARGET_SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed);
    if TARGET_FINISH_REQUESTED.load(Ordering::Relaxed) {
        TARGET_FINISH.store(true, Ordering::Relaxed);
    }
}

#[cfg(target_os = "linux")]
extern "C" fn finish_signal_handler(_signal: libc::c_int) {
    TARGET_FINISH_REQUESTED.store(true, Ordering::Relaxed);
    if !TARGET_REQUIRES_COUNT.load(Ordering::Relaxed)
        || TARGET_SIGNAL_COUNT.load(Ordering::Relaxed) > 0
    {
        TARGET_FINISH.store(true, Ordering::Relaxed);
    }
}

#[cfg(target_os = "linux")]
fn mark_runtime_ready() -> bool {
    const NAME: &[u8] = b"nub-s6-ready\0";
    unsafe { libc::prctl(libc::PR_SET_NAME, NAME.as_ptr().cast::<libc::c_char>()) == 0 }
}

#[cfg(target_os = "linux")]
fn target_exec_context_is_exact(verb: &str) -> bool {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let environment = std::env::vars_os().collect::<Vec<_>>();
    arguments.len() == 2
        && arguments[0] == "/dev/.nub-sandbox/runtime/nub-monitor"
        && arguments[1] == verb
        && std::env::current_dir().is_ok_and(|path| path == std::path::Path::new("/"))
        && environment
            == [(
                std::ffi::OsString::from("TARGET_EXEC_PROBE"),
                std::ffi::OsString::from("state5"),
            )]
}

#[cfg(target_os = "linux")]
unsafe fn park_raw() -> ! {
    loop {
        unsafe { libc::pause() };
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("the sandbox monitor harness requires Linux");
    ExitCode::from(2)
}
