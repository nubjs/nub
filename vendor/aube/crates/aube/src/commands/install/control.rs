use std::future::Future;
use std::sync::Arc;

use miette::miette;

/// How an install presents user-facing output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InstallOutputMode {
    /// Render the normal CLI progress display and human-readable summaries.
    #[default]
    Human,
    /// Suppress direct output and deliver structured events to the reporter.
    Events,
    /// Suppress both direct output and structured output events.
    Silent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPhase {
    Resolving,
    Fetching,
    Linking,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutputLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallProgressSnapshot {
    pub phase: Option<InstallPhase>,
    pub resolved: usize,
    pub total: usize,
    pub reused: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub estimated_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallEvent {
    Phase(InstallPhase),
    Progress(InstallProgressSnapshot),
    Output {
        level: InstallOutputLevel,
        code: Option<String>,
        message: String,
    },
}

/// Non-blocking destination for structured install events.
///
/// Implementations that cross a runtime boundary should enqueue and return;
/// they must not wait for the consumer while holding an install worker.
pub trait InstallReporter: Send + Sync + 'static {
    fn report(&self, event: InstallEvent);
}

#[derive(Clone)]
pub struct InstallControl {
    output: InstallOutputMode,
    reporter: Option<Arc<dyn InstallReporter>>,
    cancellation: tokio_util::sync::CancellationToken,
}

impl std::fmt::Debug for InstallControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallControl")
            .field("output", &self.output)
            .field("has_reporter", &self.reporter.is_some())
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl Default for InstallControl {
    fn default() -> Self {
        Self {
            output: InstallOutputMode::Human,
            reporter: None,
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }
}

impl InstallControl {
    pub fn events(reporter: Arc<dyn InstallReporter>) -> Self {
        Self {
            output: InstallOutputMode::Events,
            reporter: Some(reporter),
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }

    pub fn silent() -> Self {
        Self {
            output: InstallOutputMode::Silent,
            ..Self::default()
        }
    }

    pub fn output_mode(&self) -> InstallOutputMode {
        self.output
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    pub(crate) fn reporter(&self) -> Option<Arc<dyn InstallReporter>> {
        self.reporter.clone()
    }

    pub(crate) fn check_cancelled(&self) -> miette::Result<()> {
        if self.is_cancelled() {
            return Err(miette!(
                code = aube_codes::errors::ERR_AUBE_INSTALL_CANCELLED,
                "install cancelled"
            ));
        }
        Ok(())
    }

    pub(crate) fn report(&self, event: InstallEvent) {
        if let Some(reporter) = &self.reporter {
            reporter.report(event);
        }
    }

    pub(crate) fn output(
        &self,
        level: InstallOutputLevel,
        code: Option<&str>,
        message: impl Into<String>,
    ) {
        let message = message.into();
        match self.output {
            InstallOutputMode::Human => {
                let message = match level {
                    InstallOutputLevel::Info => message,
                    InstallOutputLevel::Warning => format!("warn: {message}"),
                    InstallOutputLevel::Error => format!("error: {message}"),
                };
                crate::progress::safe_eprintln(&message);
            }
            InstallOutputMode::Events => self.report(InstallEvent::Output {
                level,
                code: code.map(str::to_owned),
                message,
            }),
            InstallOutputMode::Silent => {}
        }
    }

    fn complete(&self, total: usize) {
        if self.output != InstallOutputMode::Events {
            return;
        }
        self.report(InstallEvent::Phase(InstallPhase::Complete));
        self.report(InstallEvent::Progress(InstallProgressSnapshot {
            phase: Some(InstallPhase::Complete),
            resolved: total,
            total,
            reused: total,
            downloaded: 0,
            downloaded_bytes: 0,
            estimated_bytes: 0,
        }));
    }
}

tokio::task_local! {
    static ACTIVE: InstallControl;
}

pub(crate) async fn scope<F: Future>(control: InstallControl, future: F) -> F::Output {
    ACTIVE.scope(control, future).await
}

pub(crate) fn current() -> InstallControl {
    ACTIVE.try_with(Clone::clone).unwrap_or_default()
}

pub(crate) fn check_cancelled() -> miette::Result<()> {
    current().check_cancelled()
}

pub(crate) fn output(level: InstallOutputLevel, code: Option<&str>, message: impl Into<String>) {
    current().output(level, code, message);
}

pub(crate) fn complete(total: usize) {
    current().complete(total);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingReporter(Mutex<Vec<InstallEvent>>);

    impl InstallReporter for RecordingReporter {
        fn report(&self, event: InstallEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    #[derive(Default)]
    struct CancelOnResolvingReporter(Mutex<Option<InstallControl>>);

    impl InstallReporter for CancelOnResolvingReporter {
        fn report(&self, event: InstallEvent) {
            if event == InstallEvent::Phase(InstallPhase::Resolving)
                && let Some(control) = self.0.lock().unwrap().as_ref()
            {
                control.cancel();
            }
        }
    }

    #[test]
    fn cloned_controls_share_cancellation_without_leaking_to_other_invocations() {
        let first = InstallControl::silent();
        let first_clone = first.clone();
        let second = InstallControl::silent();

        first.cancel();

        assert!(first_clone.is_cancelled());
        assert!(!second.is_cancelled());
    }

    #[tokio::test]
    async fn task_local_output_is_isolated_between_parallel_invocations() {
        let first = Arc::new(RecordingReporter::default());
        let second = Arc::new(RecordingReporter::default());
        let first_control = InstallControl::events(first.clone());
        let second_control = InstallControl::events(second.clone());

        let ((), ()) = tokio::join!(
            scope(first_control, async {
                output(InstallOutputLevel::Info, None, "first");
                tokio::task::yield_now().await;
                output(
                    InstallOutputLevel::Warning,
                    Some("WARN_FIRST"),
                    "first warning",
                );
            }),
            scope(second_control, async {
                tokio::task::yield_now().await;
                output(InstallOutputLevel::Info, None, "second");
            }),
        );

        let first_events = first.0.lock().unwrap();
        let second_events = second.0.lock().unwrap();
        assert_eq!(first_events.len(), 2);
        assert_eq!(second_events.len(), 1);
        assert!(matches!(
            &first_events[0],
            InstallEvent::Output { message, .. } if message == "first"
        ));
        assert!(matches!(
            &first_events[1],
            InstallEvent::Output { level: InstallOutputLevel::Warning, message, .. }
                if message == "first warning"
        ));
        assert!(matches!(
            &second_events[0],
            InstallEvent::Output { message, .. } if message == "second"
        ));
    }

    #[test]
    fn cancelled_control_returns_stable_error_code() {
        let control = InstallControl::silent();
        control.cancel();
        let error = control.check_cancelled().unwrap_err();

        assert_eq!(
            error.code().map(|code| code.to_string()).as_deref(),
            Some(aube_codes::errors::ERR_AUBE_INSTALL_CANCELLED)
        );
    }

    #[tokio::test]
    async fn reporter_can_cancel_an_install_before_resolution_work_starts() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join("package.json"),
            r#"{"name":"cancel-test","version":"1.0.0","dependencies":{"dep":"file:./dep"}}"#,
        )
        .unwrap();
        std::fs::create_dir(project.path().join("dep")).unwrap();
        std::fs::write(
            project.path().join("dep/package.json"),
            r#"{"name":"dep","version":"1.0.0"}"#,
        )
        .unwrap();

        let reporter = Arc::new(CancelOnResolvingReporter::default());
        let control = InstallControl::events(reporter.clone());
        *reporter.0.lock().unwrap() = Some(control.clone());
        let mut options = super::super::InstallOptions::with_mode(super::super::FrozenMode::No);
        options.project_dir = Some(project.path().to_path_buf());
        options.dry_run = true;
        options.network_mode = aube_registry::NetworkMode::Offline;
        options.control = control;

        let error = super::super::run(options).await.unwrap_err();

        assert_eq!(
            error.code().map(|code| code.to_string()).as_deref(),
            Some(aube_codes::errors::ERR_AUBE_INSTALL_CANCELLED)
        );
        assert!(!project.path().join("node_modules").exists());
    }

    #[tokio::test]
    async fn warm_path_reports_cancellation_or_completion() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join("package.json"),
            r#"{"name":"warm-test","version":"1.0.0","dependencies":{"dep":"file:./dep"}}"#,
        )
        .unwrap();
        std::fs::create_dir(project.path().join("dep")).unwrap();
        std::fs::write(
            project.path().join("dep/package.json"),
            r#"{"name":"dep","version":"1.0.0"}"#,
        )
        .unwrap();

        let mut initial = super::super::InstallOptions::with_mode(super::super::FrozenMode::No);
        initial.project_dir = Some(project.path().to_path_buf());
        initial.network_mode = aube_registry::NetworkMode::Offline;
        initial.control = InstallControl::silent();
        super::super::run(initial).await.unwrap();

        let state_path = project.path().join("node_modules/.aube-state/state.json");
        let mut state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        state
            .as_object_mut()
            .unwrap()
            .remove("package_content_hashes");
        std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();

        let cancelling_reporter = Arc::new(RecordingReporter::default());
        let cancelling_control = InstallControl::events(cancelling_reporter.clone());
        cancelling_control.cancel();
        let mut cancelled =
            super::super::InstallOptions::with_mode(super::super::FrozenMode::Prefer);
        cancelled.project_dir = Some(project.path().to_path_buf());
        cancelled.network_mode = aube_registry::NetworkMode::Offline;
        cancelled.control = cancelling_control;

        let error = super::super::run(cancelled).await.unwrap_err();
        assert_eq!(
            error.code().map(|code| code.to_string()).as_deref(),
            Some(aube_codes::errors::ERR_AUBE_INSTALL_CANCELLED)
        );
        assert!(cancelling_reporter.0.lock().unwrap().is_empty());

        let reporter = Arc::new(RecordingReporter::default());
        let mut completed =
            super::super::InstallOptions::with_mode(super::super::FrozenMode::Prefer);
        completed.project_dir = Some(project.path().to_path_buf());
        completed.network_mode = aube_registry::NetworkMode::Offline;
        completed.control = InstallControl::events(reporter.clone());
        super::super::run(completed).await.unwrap();

        let events = reporter.0.lock().unwrap();
        assert!(events.contains(&InstallEvent::Phase(InstallPhase::Complete)));
        assert!(events.iter().any(|event| matches!(
            event,
            InstallEvent::Progress(snapshot)
                if snapshot.phase == Some(InstallPhase::Complete)
                    && snapshot.resolved == 1
                    && snapshot.total == 1
                    && snapshot.reused == 1
        )));
    }
}
