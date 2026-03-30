use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Operating mode for an evolution session.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionMode {
    /// Detect issues only; do not modify code.
    Scan,
    /// Reactive: fix detected failures (test failures, compilation errors, runtime errors).
    Fix,
    /// Proactive: suggest and apply improvements (clippy, missing tests, dead code).
    Improve,
    /// Both fix and improve.
    Full,
}

impl EvolutionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Fix => "fix",
            Self::Improve => "improve",
            Self::Full => "full",
        }
    }

    pub fn includes_fix(self) -> bool {
        matches!(self, Self::Fix | Self::Full)
    }

    pub fn includes_improve(self) -> bool {
        matches!(self, Self::Improve | Self::Full)
    }
}

/// What triggered an evolution cycle.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum EvolutionTrigger {
    Manual,
    TestFailure { test_name: String, error: String },
    RuntimeError { context: String, error: String },
    Scheduled { schedule_id: String },
}

impl EvolutionTrigger {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::TestFailure { .. } => "test_failure",
            Self::RuntimeError { .. } => "runtime_error",
            Self::Scheduled { .. } => "scheduled",
        }
    }
}

/// Phase of the evolution cycle FSM.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionPhase {
    Detect,
    Diagnose,
    Snapshot,
    Patch,
    Verify,
    Decide,
}

impl EvolutionPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Detect => "detect",
            Self::Diagnose => "diagnose",
            Self::Snapshot => "snapshot",
            Self::Patch => "patch",
            Self::Verify => "verify",
            Self::Decide => "decide",
        }
    }
}

/// Final decision for an evolution attempt.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum EvolutionDecision {
    Keep {
        commit_sha: String,
    },
    Rollback {
        reason: String,
        breadcrumb_id: String,
    },
    Skip {
        reason: String,
    },
}

/// Classification of a detected issue.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionIssueKind {
    TestFailure,
    CompilationError,
    ClippyWarning,
    MissingTest,
    DeadCode,
    RuntimeError,
}

impl EvolutionIssueKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TestFailure => "test_failure",
            Self::CompilationError => "compilation_error",
            Self::ClippyWarning => "clippy_warning",
            Self::MissingTest => "missing_test",
            Self::DeadCode => "dead_code",
            Self::RuntimeError => "runtime_error",
        }
    }

    pub fn is_reactive(self) -> bool {
        matches!(
            self,
            Self::TestFailure | Self::CompilationError | Self::RuntimeError
        )
    }
}

/// Severity of a detected issue.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    Low,
    Medium,
    High,
    Critical,
}

/// A single issue detected during the DETECT phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvolutionIssue {
    pub issue_id: String,
    pub kind: EvolutionIssueKind,
    pub file_path: String,
    pub description: String,
    pub severity: IssueSeverity,
    pub raw_output: Option<String>,
}

/// A breadcrumb recording what was tried and the outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvolutionBreadcrumb {
    pub breadcrumb_id: String,
    pub session_id: String,
    pub issue_id: String,
    pub attempt_number: u32,
    pub phase_reached: EvolutionPhase,
    pub decision: EvolutionDecision,
    pub patch_description: String,
    pub error_output: Option<String>,
    pub files_modified: Vec<String>,
    pub fingerprint_before: String,
    pub fingerprint_after: Option<String>,
    pub timestamp_epoch_s: i64,
}

/// Summary of an evolution session (groups related attempts).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvolutionSessionSummary {
    pub session_id: String,
    pub mode: EvolutionMode,
    pub trigger: EvolutionTrigger,
    pub started_at_epoch_s: i64,
    pub completed_at_epoch_s: Option<i64>,
    pub issues_detected: u32,
    pub patches_applied: u32,
    pub patches_rolled_back: u32,
    pub patches_skipped: u32,
}

/// Configuration for the evolution engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvolutionConfig {
    pub max_retries_per_issue: u32,
    pub max_mutations_per_session: u32,
    pub cooldown_seconds: u64,
    pub auto_commit: bool,
    pub dry_run: bool,
    pub verify_commands: Vec<String>,
}

impl Default for EvolutionConfig {
    fn default() -> Self {
        Self {
            max_retries_per_issue: 3,
            max_mutations_per_session: 10,
            cooldown_seconds: 300,
            auto_commit: true,
            dry_run: false,
            verify_commands: vec![
                "cargo test --workspace".to_owned(),
                "cargo clippy --workspace --all-targets --all-features -- -D warnings".to_owned(),
            ],
        }
    }
}

/// Output from a shell command execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl ShellOutput {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// Structured report from a completed evolution session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvolutionReport {
    pub session: EvolutionSessionSummary,
    pub breadcrumbs: Vec<EvolutionBreadcrumb>,
    pub issues: Vec<EvolutionIssue>,
    pub metadata: BTreeMap<String, String>,
}
