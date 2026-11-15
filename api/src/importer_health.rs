//! Importer health for the startup check, the health poll and the gates. The
//! importer keeps the cached result; these only decide when to check again.

use shared::failure::Failure;
use shared::system::{HealthLevel, ImporterHealth};
use soulbeet::MusicImporter;
use std::sync::Once;
use tracing::error;

use crate::import_queue::import_failure;
use crate::services::music_importer;

static STARTUP_PROBE: Once = Once::new();

/// Checks the importer once in the background. Never awaited: the serve
/// callback runs before the server accepts connections.
pub fn start_startup_probe() {
    STARTUP_PROBE.call_once(|| {
        tokio::spawn(async {
            match music_importer(None).await {
                Ok(importer) => {
                    importer.refresh_health().await;
                }
                Err(e) => error!("No importer to check: {e}"),
            }
        });
    });
}

/// The cached health. Starts a check in the background when what the last
/// one depended on has changed.
pub async fn snapshot() -> ImporterHealth {
    let importer = match music_importer(None).await {
        Ok(importer) => importer,
        Err(e) => {
            error!("No importer to report on: {e}");
            return ImporterHealth::default();
        }
    };
    let health = importer.health();
    if importer.health_stale().await {
        tokio::spawn(async move {
            importer.refresh_health().await;
        });
    }
    health
}

/// The failure a job would end with because `importer` is broken, from its
/// cached health: `import_failure(&importer.refused(issue.title))`. None when
/// nothing blocks imports.
pub fn blocking_failure_of(importer: &dyn MusicImporter) -> Option<Failure> {
    let health = importer.health();
    let issue = health.blocking_issue()?;
    import_failure(&importer.refused(issue.title.clone()))
}

/// `blocking_failure_of` the default importer; None when there is no importer
/// to ask.
pub async fn blocking_failure() -> Option<Failure> {
    blocking_failure_of(music_importer(None).await.ok()?.as_ref())
}

/// Why `importer` must not run a job, if it must not. A Broken importer is
/// checked again first, so a fixed config does not wait for the next poll,
/// and one not checked yet is checked before its first job.
pub async fn refusal(importer: &dyn MusicImporter) -> Option<String> {
    if !matches!(
        importer.health().level,
        HealthLevel::Broken | HealthLevel::Checking
    ) {
        return None;
    }
    importer
        .refresh_health()
        .await
        .blocking_issue()
        .map(|issue| issue.title.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::failure::{FailureCode, RetryAdvice, Stage};
    use shared::library::DuplicateReport;
    use shared::system::HealthIssue;
    use soulbeet::{DriverChoice, DuplicateRun, ForgetOutcome, ImportMode, ImportResult};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct Stub {
        health: Mutex<ImporterHealth>,
        after_refresh: ImporterHealth,
        refreshes: AtomicUsize,
    }

    impl Stub {
        fn new(now: ImporterHealth, after_refresh: ImporterHealth) -> Self {
            Stub {
                health: Mutex::new(now),
                after_refresh,
                refreshes: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl MusicImporter for Stub {
        fn id(&self) -> &'static str {
            "stub"
        }
        fn name(&self) -> &'static str {
            "Stub"
        }
        async fn import(
            &self,
            _: &[&Path],
            _: &Path,
            _: ImportMode,
            _: &DriverChoice,
            _: DuplicateRun,
        ) -> ImportResult {
            unreachable!()
        }
        async fn relocate(&self, _: &Path, _: &[u64]) -> Result<Vec<(u64, PathBuf)>, String> {
            unreachable!()
        }
        fn refused(&self, _: String) -> ImportResult {
            unreachable!()
        }
        async fn find_duplicates(&self, _: &[&Path]) -> Result<DuplicateReport, String> {
            unreachable!()
        }
        async fn forget(&self, _: &Path, _: &Path) -> ForgetOutcome {
            unreachable!()
        }
        fn health(&self) -> ImporterHealth {
            self.health.lock().unwrap().clone()
        }
        async fn refresh_health(&self) -> ImporterHealth {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            *self.health.lock().unwrap() = self.after_refresh.clone();
            self.after_refresh.clone()
        }
        async fn health_stale(&self) -> bool {
            false
        }
    }

    fn health(level: HealthLevel) -> ImporterHealth {
        let issues = match level {
            HealthLevel::Broken => vec![HealthIssue {
                severity: HealthLevel::Broken,
                title: "No metadata source plugin loaded".into(),
                detail: String::new(),
            }],
            _ => vec![],
        };
        ImporterHealth {
            level,
            issues,
            facts: vec![],
        }
    }

    #[test]
    fn a_broken_importer_refuses_with_its_own_failure() {
        use crate::import_queue::test_support::{broken, ready, StubImporter};
        let stub = StubImporter::new();
        stub.set_health(broken());
        let f = blocking_failure_of(&stub).expect("a broken importer blocks");
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::ImporterBroken,
                Stage::Import,
                RetryAdvice::NeedsUser,
                "No metadata source plugin loaded"
            )
        );
        stub.set_health(ready());
        assert_eq!(blocking_failure_of(&stub), None);
    }

    #[tokio::test]
    async fn a_ready_importer_is_not_checked_again() {
        let stub = Stub::new(health(HealthLevel::Ready), health(HealthLevel::Broken));
        assert_eq!(refusal(&stub).await, None);
        assert_eq!(stub.refreshes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_broken_importer_is_refused_with_the_issue_title_if_still_broken() {
        let stub = Stub::new(health(HealthLevel::Broken), health(HealthLevel::Broken));
        assert_eq!(
            refusal(&stub).await.as_deref(),
            Some("No metadata source plugin loaded")
        );
        assert_eq!(stub.refreshes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_importer_not_checked_yet_is_checked_before_a_job() {
        let stub = Stub::new(health(HealthLevel::Checking), health(HealthLevel::Broken));
        assert_eq!(
            refusal(&stub).await.as_deref(),
            Some("No metadata source plugin loaded")
        );
        assert_eq!(stub.refreshes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_degraded_importer_is_not_checked_again() {
        let stub = Stub::new(health(HealthLevel::Degraded), health(HealthLevel::Broken));
        assert_eq!(refusal(&stub).await, None);
        assert_eq!(stub.refreshes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_broken_importer_that_was_fixed_is_not_refused() {
        let stub = Stub::new(health(HealthLevel::Broken), health(HealthLevel::Ready));
        assert_eq!(refusal(&stub).await, None);
    }
}
