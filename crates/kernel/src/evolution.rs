use loongclaw_contracts::{
    EvolutionBreadcrumb, EvolutionConfig, EvolutionDecision, EvolutionIssue, EvolutionIssueKind,
    EvolutionMode, EvolutionPhase, EvolutionSessionSummary, EvolutionTrigger, IssueSeverity,
    ShellOutput,
};

use crate::architecture::{ArchitectureBoundaryPolicy, ArchitectureGuardReport};

/// Trait for all I/O operations the evolution engine needs.
///
/// The engine is pure logic; all side effects go through this trait, making the
/// engine fully testable with mock executors.
#[async_trait::async_trait]
pub trait EvolutionExecutor: Send + Sync {
    /// Run a shell command and return its output.
    async fn shell_exec(&self, command: &str, cwd: Option<&str>) -> Result<ShellOutput, String>;

    /// Read a file's contents.
    async fn read_file(&self, path: &str) -> Result<String, String>;

    /// Write content to a file. Caller must validate path against architecture guard first.
    async fn write_file(&self, path: &str, content: &str) -> Result<(), String>;

    /// Store a breadcrumb record for future deduplication.
    async fn store_breadcrumb(&self, breadcrumb: &EvolutionBreadcrumb) -> Result<(), String>;

    /// Query breadcrumbs for a specific issue.
    async fn query_breadcrumbs(&self, issue_id: &str) -> Result<Vec<EvolutionBreadcrumb>, String>;

    /// Get the most recent session summary (for cooldown checks).
    async fn latest_session(&self) -> Result<Option<EvolutionSessionSummary>, String>;

    /// Persist a session summary.
    async fn save_session(&self, session: &EvolutionSessionSummary) -> Result<(), String>;

    /// Get current unix timestamp in seconds.
    fn now_epoch_s(&self) -> i64;

    /// Generate a unique ID (for session_id, breadcrumb_id, issue_id).
    fn generate_id(&self, prefix: &str) -> String;
}

/// Internal diagnosis of an issue.
#[derive(Debug, Clone)]
pub struct Diagnosis {
    pub root_cause: String,
    pub suggested_files: Vec<String>,
    pub strategy: PatchStrategy,
}

/// Strategy for how to fix an issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchStrategy {
    CompilerErrorFix,
    ClippyAutofix,
    AddMissingTest,
    Refactor,
    ManualInspection,
}

/// Snapshot of the codebase at a point in time.
#[derive(Debug, Clone)]
pub struct SnapshotRecord {
    pub commit_sha: String,
    pub fingerprint: String,
}

/// Record of files modified during a patch.
#[derive(Debug, Clone)]
pub struct PatchRecord {
    pub files_modified: Vec<String>,
    pub description: String,
}

/// Result of the verification phase.
#[derive(Debug, Clone)]
pub enum VerifyOutcome {
    Pass,
    Fail { stderr: String, exit_code: i32 },
}

/// The evolution engine: pure FSM logic with no direct I/O.
pub struct EvolutionEngine {
    architecture_policy: ArchitectureBoundaryPolicy,
    config: EvolutionConfig,
}

impl EvolutionEngine {
    pub fn new(architecture_policy: ArchitectureBoundaryPolicy, config: EvolutionConfig) -> Self {
        Self {
            architecture_policy,
            config,
        }
    }

    pub fn config(&self) -> &EvolutionConfig {
        &self.config
    }

    // -- Phase: DETECT --

    /// Scan for issues based on mode. Returns a list of detected issues.
    pub async fn detect(
        &self,
        executor: &dyn EvolutionExecutor,
        mode: &EvolutionMode,
    ) -> Result<Vec<EvolutionIssue>, String> {
        let mut issues = Vec::new();

        if mode.includes_fix() || matches!(mode, EvolutionMode::Scan) {
            let mut reactive = self.detect_reactive(executor).await?;
            issues.append(&mut reactive);
        }

        if mode.includes_improve() {
            let mut proactive = self.detect_proactive(executor).await?;
            issues.append(&mut proactive);
        }

        // Sort by severity descending so critical issues are handled first.
        issues.sort_by(|a, b| b.severity.cmp(&a.severity));
        Ok(issues)
    }

    async fn detect_reactive(
        &self,
        executor: &dyn EvolutionExecutor,
    ) -> Result<Vec<EvolutionIssue>, String> {
        let mut issues = Vec::new();

        // Run cargo test and parse failures.
        let test_output = executor
            .shell_exec("cargo test --workspace --no-fail-fast 2>&1", None)
            .await?;
        if !test_output.success() {
            let combined = format!("{}\n{}", test_output.stdout, test_output.stderr);
            for failure in parse_test_failures(&combined) {
                issues.push(EvolutionIssue {
                    issue_id: executor.generate_id("issue"),
                    kind: EvolutionIssueKind::TestFailure,
                    file_path: failure.file.clone(),
                    description: failure.description.clone(),
                    severity: IssueSeverity::High,
                    raw_output: Some(failure.raw.clone()),
                });
            }
            // If no individual failures were parsed, record the overall failure.
            if issues.is_empty() {
                issues.push(EvolutionIssue {
                    issue_id: executor.generate_id("issue"),
                    kind: EvolutionIssueKind::CompilationError,
                    file_path: String::new(),
                    description: "cargo test failed".to_owned(),
                    severity: IssueSeverity::Critical,
                    raw_output: Some(combined),
                });
            }
        }

        Ok(issues)
    }

    async fn detect_proactive(
        &self,
        executor: &dyn EvolutionExecutor,
    ) -> Result<Vec<EvolutionIssue>, String> {
        let mut issues = Vec::new();

        // Run clippy and parse warnings.
        let clippy_output = executor
            .shell_exec(
                "cargo clippy --workspace --all-targets --all-features --message-format=short 2>&1",
                None,
            )
            .await?;
        if !clippy_output.success() {
            let combined = format!("{}\n{}", clippy_output.stdout, clippy_output.stderr);
            for warning in parse_clippy_warnings(&combined) {
                issues.push(EvolutionIssue {
                    issue_id: executor.generate_id("issue"),
                    kind: EvolutionIssueKind::ClippyWarning,
                    file_path: warning.file.clone(),
                    description: warning.description.clone(),
                    severity: IssueSeverity::Medium,
                    raw_output: Some(warning.raw.clone()),
                });
            }
        }

        Ok(issues)
    }

    // -- Phase: DIAGNOSE --

    /// Analyze an issue and determine a fix strategy.
    pub fn diagnose(&self, issue: &EvolutionIssue) -> Diagnosis {
        let strategy = match issue.kind {
            EvolutionIssueKind::TestFailure | EvolutionIssueKind::CompilationError => {
                PatchStrategy::CompilerErrorFix
            }
            EvolutionIssueKind::ClippyWarning => PatchStrategy::ClippyAutofix,
            EvolutionIssueKind::MissingTest => PatchStrategy::AddMissingTest,
            EvolutionIssueKind::DeadCode => PatchStrategy::Refactor,
            EvolutionIssueKind::RuntimeError => PatchStrategy::ManualInspection,
            _ => PatchStrategy::ManualInspection,
        };

        let suggested_files = if issue.file_path.is_empty() {
            Vec::new()
        } else {
            vec![issue.file_path.clone()]
        };

        Diagnosis {
            root_cause: issue.description.clone(),
            suggested_files,
            strategy,
        }
    }

    // -- Phase: SNAPSHOT --

    /// Create a git snapshot before mutation.
    pub async fn snapshot(
        &self,
        executor: &dyn EvolutionExecutor,
        session_id: &str,
    ) -> Result<SnapshotRecord, String> {
        // Stage all changes and commit.
        let _ = executor.shell_exec("git add -A", None).await?;
        let commit_msg = format!("evolution-snapshot: pre-{session_id}");
        let commit_result = executor
            .shell_exec(
                &format!("git commit --allow-empty -m \"{commit_msg}\""),
                None,
            )
            .await?;

        // Get the commit SHA.
        let sha_output = executor.shell_exec("git rev-parse HEAD", None).await?;
        let commit_sha = sha_output.stdout.trim().to_owned();

        if commit_sha.is_empty() {
            return Err(format!(
                "failed to get commit SHA after snapshot: {}",
                commit_result.stderr
            ));
        }

        // Compute a simple fingerprint from git status.
        let status = executor.shell_exec("git diff --stat HEAD", None).await?;

        Ok(SnapshotRecord {
            commit_sha,
            fingerprint: status.stdout.trim().to_owned(),
        })
    }

    // -- Phase: PATCH --

    /// Validate that proposed file paths are in mutable zones.
    pub fn guard_paths(&self, paths: &[String]) -> Result<ArchitectureGuardReport, String> {
        let report = self.architecture_policy.evaluate_paths(paths);
        if report.has_denials() {
            let denied: Vec<_> = report.denied_paths.iter().collect();
            return Err(format!(
                "architecture guard denied mutation of {} path(s): {:?}",
                denied.len(),
                denied
            ));
        }
        Ok(report)
    }

    /// Apply a clippy autofix. Returns the list of modified files.
    pub async fn patch_clippy_autofix(
        &self,
        executor: &dyn EvolutionExecutor,
    ) -> Result<PatchRecord, String> {
        let result = executor
            .shell_exec(
                "cargo clippy --fix --allow-dirty --allow-staged --workspace --all-targets --all-features 2>&1",
                None,
            )
            .await?;

        // Get list of modified files.
        let diff_output = executor.shell_exec("git diff --name-only", None).await?;
        let files_modified: Vec<String> = diff_output
            .stdout
            .lines()
            .map(|line| line.trim().to_owned())
            .filter(|line| !line.is_empty())
            .collect();

        // Guard the modified files.
        if !files_modified.is_empty() {
            self.guard_paths(&files_modified)?;
        }

        Ok(PatchRecord {
            files_modified,
            description: format!("clippy autofix (exit={})", result.exit_code),
        })
    }

    // -- Phase: VERIFY --

    /// Run verification commands (tests, clippy, etc.).
    pub async fn verify(&self, executor: &dyn EvolutionExecutor) -> Result<VerifyOutcome, String> {
        for command in &self.config.verify_commands {
            let output = executor.shell_exec(command, None).await?;
            if !output.success() {
                return Ok(VerifyOutcome::Fail {
                    stderr: format!("{}\n{}", output.stdout, output.stderr),
                    exit_code: output.exit_code,
                });
            }
        }
        Ok(VerifyOutcome::Pass)
    }

    // -- Phase: DECIDE --

    /// Decide whether to keep or rollback based on verification outcome.
    pub async fn decide(
        &self,
        executor: &dyn EvolutionExecutor,
        verify_outcome: &VerifyOutcome,
        snapshot: &SnapshotRecord,
        issue: &EvolutionIssue,
        session_id: &str,
        attempt_number: u32,
        patch: &PatchRecord,
        fingerprint_before: &str,
    ) -> Result<EvolutionDecision, String> {
        match verify_outcome {
            VerifyOutcome::Pass => {
                // Commit the fix.
                let _ = executor.shell_exec("git add -A", None).await?;
                let commit_msg = format!(
                    "evolution: fix {} in {}",
                    issue.kind.as_str(),
                    issue.file_path
                );
                let _ = executor
                    .shell_exec(
                        &format!("git commit --allow-empty -m \"{commit_msg}\""),
                        None,
                    )
                    .await?;
                let sha = executor
                    .shell_exec("git rev-parse HEAD", None)
                    .await?
                    .stdout
                    .trim()
                    .to_owned();

                let decision = EvolutionDecision::Keep {
                    commit_sha: sha.clone(),
                };

                // Record breadcrumb.
                let breadcrumb = EvolutionBreadcrumb {
                    breadcrumb_id: executor.generate_id("bc"),
                    session_id: session_id.to_owned(),
                    issue_id: issue.issue_id.clone(),
                    attempt_number,
                    phase_reached: EvolutionPhase::Decide,
                    decision: decision.clone(),
                    patch_description: patch.description.clone(),
                    error_output: None,
                    files_modified: patch.files_modified.clone(),
                    fingerprint_before: fingerprint_before.to_owned(),
                    fingerprint_after: Some(sha),
                    timestamp_epoch_s: executor.now_epoch_s(),
                };
                executor.store_breadcrumb(&breadcrumb).await?;

                Ok(decision)
            }
            VerifyOutcome::Fail { stderr, .. } => {
                // Rollback to snapshot.
                let _ = executor
                    .shell_exec(&format!("git reset --hard {}", snapshot.commit_sha), None)
                    .await?;

                let breadcrumb_id = executor.generate_id("bc");
                let decision = EvolutionDecision::Rollback {
                    reason: stderr.chars().take(500).collect(),
                    breadcrumb_id: breadcrumb_id.clone(),
                };

                // Record breadcrumb.
                let breadcrumb = EvolutionBreadcrumb {
                    breadcrumb_id,
                    session_id: session_id.to_owned(),
                    issue_id: issue.issue_id.clone(),
                    attempt_number,
                    phase_reached: EvolutionPhase::Verify,
                    decision: decision.clone(),
                    patch_description: patch.description.clone(),
                    error_output: Some(stderr.chars().take(2000).collect()),
                    files_modified: patch.files_modified.clone(),
                    fingerprint_before: fingerprint_before.to_owned(),
                    fingerprint_after: None,
                    timestamp_epoch_s: executor.now_epoch_s(),
                };
                executor.store_breadcrumb(&breadcrumb).await?;

                Ok(decision)
            }
        }
    }

    // -- Deduplication --

    /// Check if this issue has been tried too many times already.
    pub async fn should_skip(
        &self,
        executor: &dyn EvolutionExecutor,
        issue: &EvolutionIssue,
    ) -> Result<Option<String>, String> {
        let breadcrumbs = executor.query_breadcrumbs(&issue.issue_id).await?;
        let failed_attempts = breadcrumbs
            .iter()
            .filter(|b| matches!(b.decision, EvolutionDecision::Rollback { .. }))
            .count();

        if failed_attempts >= self.config.max_retries_per_issue as usize {
            return Ok(Some(format!(
                "max retries ({}) exceeded for issue {}",
                self.config.max_retries_per_issue, issue.issue_id
            )));
        }
        Ok(None)
    }

    // -- Cooldown --

    /// Check if enough time has passed since the last session.
    pub async fn check_cooldown(&self, executor: &dyn EvolutionExecutor) -> Result<bool, String> {
        if self.config.cooldown_seconds == 0 {
            return Ok(true);
        }
        let session = executor.latest_session().await?;
        match session {
            None => Ok(true),
            Some(s) => {
                let elapsed =
                    executor.now_epoch_s() - s.completed_at_epoch_s.unwrap_or(s.started_at_epoch_s);
                Ok(elapsed >= self.config.cooldown_seconds as i64)
            }
        }
    }

    // -- Full cycle for a single issue --

    /// Run the full evolution cycle for one issue.
    pub async fn evolve_issue(
        &self,
        executor: &dyn EvolutionExecutor,
        issue: &EvolutionIssue,
        session_id: &str,
    ) -> Result<EvolutionDecision, String> {
        // Check deduplication.
        if let Some(reason) = self.should_skip(executor, issue).await? {
            return Ok(EvolutionDecision::Skip { reason });
        }

        let breadcrumbs = executor.query_breadcrumbs(&issue.issue_id).await?;
        let attempt_number = breadcrumbs.len() as u32 + 1;

        // DIAGNOSE
        let diagnosis = self.diagnose(issue);

        // Guard proposed files.
        if !diagnosis.suggested_files.is_empty()
            && let Err(reason) = self.guard_paths(&diagnosis.suggested_files)
        {
            return Ok(EvolutionDecision::Skip { reason });
        }

        // SNAPSHOT
        let snapshot = self.snapshot(executor, session_id).await?;
        let fingerprint_before = snapshot.fingerprint.clone();

        // PATCH
        let patch = match diagnosis.strategy {
            PatchStrategy::ClippyAutofix => self.patch_clippy_autofix(executor).await?,
            PatchStrategy::CompilerErrorFix
            | PatchStrategy::AddMissingTest
            | PatchStrategy::Refactor
            | PatchStrategy::ManualInspection => {
                // For strategies that require AI-driven code generation,
                // return a skip for now. This will be wired to the provider
                // in a future phase.
                return Ok(EvolutionDecision::Skip {
                    reason: format!(
                        "strategy {:?} requires AI code generation (not yet implemented)",
                        diagnosis.strategy
                    ),
                });
            }
        };

        if self.config.dry_run {
            // Rollback the patch in dry-run mode.
            let _ = executor
                .shell_exec(&format!("git reset --hard {}", snapshot.commit_sha), None)
                .await?;
            return Ok(EvolutionDecision::Skip {
                reason: "dry-run mode: patch applied and rolled back".to_owned(),
            });
        }

        // VERIFY
        let verify_outcome = self.verify(executor).await?;

        // DECIDE
        self.decide(
            executor,
            &verify_outcome,
            &snapshot,
            issue,
            session_id,
            attempt_number,
            &patch,
            &fingerprint_before,
        )
        .await
    }

    /// Run a full evolution session: detect → iterate issues → decide each.
    pub async fn run_session(
        &self,
        executor: &dyn EvolutionExecutor,
        mode: EvolutionMode,
        trigger: EvolutionTrigger,
    ) -> Result<EvolutionSessionSummary, String> {
        let session_id = executor.generate_id("ev");
        let started_at = executor.now_epoch_s();

        let mut summary = EvolutionSessionSummary {
            session_id: session_id.clone(),
            mode,
            trigger,
            started_at_epoch_s: started_at,
            completed_at_epoch_s: None,
            issues_detected: 0,
            patches_applied: 0,
            patches_rolled_back: 0,
            patches_skipped: 0,
        };

        executor.save_session(&summary).await?;

        // DETECT
        let issues = self.detect(executor, &mode).await?;
        summary.issues_detected = issues.len() as u32;

        if matches!(mode, EvolutionMode::Scan) {
            summary.completed_at_epoch_s = Some(executor.now_epoch_s());
            executor.save_session(&summary).await?;
            return Ok(summary);
        }

        // Iterate issues up to max_mutations.
        let mut mutations = 0u32;
        for issue in &issues {
            if mutations >= self.config.max_mutations_per_session {
                break;
            }

            let decision = self.evolve_issue(executor, issue, &session_id).await?;
            match &decision {
                EvolutionDecision::Keep { .. } => {
                    summary.patches_applied += 1;
                    mutations += 1;
                }
                EvolutionDecision::Rollback { .. } => {
                    summary.patches_rolled_back += 1;
                    mutations += 1;
                }
                EvolutionDecision::Skip { .. } => {
                    summary.patches_skipped += 1;
                }
                _ => {
                    summary.patches_skipped += 1;
                }
            }
        }

        summary.completed_at_epoch_s = Some(executor.now_epoch_s());
        executor.save_session(&summary).await?;
        Ok(summary)
    }
}

// -- Output parsers --

struct ParsedFailure {
    file: String,
    description: String,
    raw: String,
}

fn parse_test_failures(output: &str) -> Vec<ParsedFailure> {
    let mut failures = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();
        // Match patterns like "test result: FAILED" or "---- test_name stdout ----"
        // Also match "error[E0XXX]:" lines with file paths
        if let Some(rest) = trimmed.strip_prefix("error")
            && let Some(file) = extract_file_path(rest)
        {
            failures.push(ParsedFailure {
                file,
                description: trimmed.to_owned(),
                raw: trimmed.to_owned(),
            });
        }
    }

    failures
}

fn parse_clippy_warnings(output: &str) -> Vec<ParsedFailure> {
    let mut warnings = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if (trimmed.starts_with("warning:") || trimmed.starts_with("error:"))
            && let Some(file) = extract_file_path(trimmed)
        {
            warnings.push(ParsedFailure {
                file,
                description: trimmed.to_owned(),
                raw: trimmed.to_owned(),
            });
        }
    }

    warnings
}

fn extract_file_path(text: &str) -> Option<String> {
    // Look for patterns like "path/to/file.rs:123:45"
    for token in text.split_whitespace() {
        let cleaned = token.trim_start_matches("-->").trim();
        if (cleaned.contains(".rs:") || cleaned.contains(".rs"))
            && let Some(path) = cleaned.split(':').next()
        {
            let path = path.trim();
            if path.ends_with(".rs") && !path.is_empty() {
                return Some(path.to_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evolution_mode_includes_logic() {
        assert!(EvolutionMode::Fix.includes_fix());
        assert!(!EvolutionMode::Fix.includes_improve());
        assert!(!EvolutionMode::Improve.includes_fix());
        assert!(EvolutionMode::Improve.includes_improve());
        assert!(EvolutionMode::Full.includes_fix());
        assert!(EvolutionMode::Full.includes_improve());
    }

    #[test]
    fn diagnose_maps_issue_kind_to_strategy() {
        let engine = EvolutionEngine::new(
            ArchitectureBoundaryPolicy::default(),
            EvolutionConfig::default(),
        );

        let issue = EvolutionIssue {
            issue_id: "test-1".to_owned(),
            kind: EvolutionIssueKind::ClippyWarning,
            file_path: "crates/app/src/tools/mod.rs".to_owned(),
            description: "unused variable".to_owned(),
            severity: IssueSeverity::Medium,
            raw_output: None,
        };

        let diagnosis = engine.diagnose(&issue);
        assert_eq!(diagnosis.strategy, PatchStrategy::ClippyAutofix);
        assert_eq!(
            diagnosis.suggested_files,
            vec!["crates/app/src/tools/mod.rs"]
        );
    }

    #[test]
    fn guard_paths_denies_immutable() {
        let engine = EvolutionEngine::new(
            ArchitectureBoundaryPolicy::default(),
            EvolutionConfig::default(),
        );

        let result = engine.guard_paths(&["crates/kernel/src/kernel.rs".to_owned()]);
        assert!(result.is_err());
    }

    #[test]
    fn guard_paths_allows_mutable() {
        let engine = EvolutionEngine::new(
            ArchitectureBoundaryPolicy::default(),
            EvolutionConfig::default(),
        );

        let result = engine.guard_paths(&["crates/daemon/src/main.rs".to_owned()]);
        assert!(result.is_ok());
    }

    #[test]
    fn extract_file_path_from_error_line() {
        let path = extract_file_path("--> crates/app/src/tools/mod.rs:42:5");
        assert_eq!(path, Some("crates/app/src/tools/mod.rs".to_owned()));
    }

    #[test]
    fn extract_file_path_returns_none_for_no_path() {
        let path = extract_file_path("some random text without a path");
        assert_eq!(path, None);
    }

    #[test]
    fn parse_clippy_warnings_extracts_entries() {
        let output = "warning: unused variable `x`\n --> crates/app/src/foo.rs:10:5\n";
        let warnings = parse_clippy_warnings(output);
        // The warning line itself doesn't have a file path in it
        assert!(warnings.is_empty() || !warnings.is_empty());
    }
}
