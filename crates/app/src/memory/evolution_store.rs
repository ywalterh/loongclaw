use std::path::{Path, PathBuf};
use std::sync::Mutex;

use loongclaw_contracts::{
    EvolutionBreadcrumb, EvolutionDecision, EvolutionMode, EvolutionPhase, EvolutionSessionSummary,
    EvolutionTrigger,
};
use rusqlite::Connection;

const SCHEMA_VERSION: i32 = 1;

/// SQLite-backed store for evolution breadcrumbs and session summaries.
///
/// Separate from the conversation memory database to avoid schema coupling.
pub struct EvolutionBreadcrumbStore {
    conn: Mutex<Connection>,
}

impl EvolutionBreadcrumbStore {
    /// Open or create the evolution store at the given path.
    pub fn open(db_path: &Path) -> Result<Self, String> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create evolution store directory: {e}"))?;
        }
        let conn = Connection::open(db_path)
            .map_err(|e| format!("failed to open evolution store: {e}"))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .map_err(|e| format!("failed to configure evolution store: {e}"))?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// Open an in-memory store (for testing).
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory()
            .map_err(|e| format!("failed to open in-memory evolution store: {e}"))?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.init_schema()?;
        Ok(store)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, String> {
        self.conn
            .lock()
            .map_err(|e| format!("evolution store mutex poisoned: {e}"))
    }

    fn init_schema(&self) -> Result<(), String> {
        let conn = self.lock()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS evolution_sessions (
                session_id TEXT PRIMARY KEY,
                mode TEXT NOT NULL,
                trigger_kind TEXT NOT NULL,
                trigger_detail TEXT,
                started_at_epoch_s INTEGER NOT NULL,
                completed_at_epoch_s INTEGER,
                issues_detected INTEGER NOT NULL DEFAULT 0,
                patches_applied INTEGER NOT NULL DEFAULT 0,
                patches_rolled_back INTEGER NOT NULL DEFAULT 0,
                patches_skipped INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS evolution_breadcrumbs (
                breadcrumb_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                issue_id TEXT NOT NULL,
                attempt_number INTEGER NOT NULL,
                phase_reached TEXT NOT NULL,
                decision_json TEXT NOT NULL,
                patch_description TEXT NOT NULL DEFAULT '',
                error_output TEXT,
                files_modified_json TEXT NOT NULL DEFAULT '[]',
                fingerprint_before TEXT NOT NULL DEFAULT '',
                fingerprint_after TEXT,
                timestamp_epoch_s INTEGER NOT NULL,
                FOREIGN KEY (session_id) REFERENCES evolution_sessions(session_id)
            );

            CREATE INDEX IF NOT EXISTS idx_breadcrumbs_issue_id
                ON evolution_breadcrumbs(issue_id);
            CREATE INDEX IF NOT EXISTS idx_breadcrumbs_session_id
                ON evolution_breadcrumbs(session_id);

            CREATE TABLE IF NOT EXISTS evolution_schema_version (
                version INTEGER NOT NULL
            );",
        )
        .map_err(|e| format!("failed to create evolution schema: {e}"))?;

        // Insert schema version if not present.
        let count: i32 = conn
            .query_row("SELECT COUNT(*) FROM evolution_schema_version", [], |row| {
                row.get(0)
            })
            .map_err(|e| format!("failed to check schema version: {e}"))?;
        if count == 0 {
            conn.execute(
                "INSERT INTO evolution_schema_version (version) VALUES (?1)",
                [SCHEMA_VERSION],
            )
            .map_err(|e| format!("failed to insert schema version: {e}"))?;
        }

        Ok(())
    }

    /// Insert a breadcrumb record.
    pub fn insert_breadcrumb(&self, b: &EvolutionBreadcrumb) -> Result<(), String> {
        let conn = self.lock()?;
        let decision_json = serde_json::to_string(&b.decision)
            .map_err(|e| format!("failed to serialize decision: {e}"))?;
        let files_json = serde_json::to_string(&b.files_modified)
            .map_err(|e| format!("failed to serialize files: {e}"))?;

        conn.execute(
            "INSERT OR REPLACE INTO evolution_breadcrumbs
             (breadcrumb_id, session_id, issue_id, attempt_number, phase_reached,
              decision_json, patch_description, error_output, files_modified_json,
              fingerprint_before, fingerprint_after, timestamp_epoch_s)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                b.breadcrumb_id,
                b.session_id,
                b.issue_id,
                b.attempt_number,
                b.phase_reached.as_str(),
                decision_json,
                b.patch_description,
                b.error_output,
                files_json,
                b.fingerprint_before,
                b.fingerprint_after,
                b.timestamp_epoch_s,
            ],
        )
        .map_err(|e| format!("failed to insert breadcrumb: {e}"))?;
        Ok(())
    }

    /// Query breadcrumbs for a specific issue.
    pub fn query_by_issue(&self, issue_id: &str) -> Result<Vec<EvolutionBreadcrumb>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT breadcrumb_id, session_id, issue_id, attempt_number, phase_reached,
                        decision_json, patch_description, error_output, files_modified_json,
                        fingerprint_before, fingerprint_after, timestamp_epoch_s
                 FROM evolution_breadcrumbs
                 WHERE issue_id = ?1
                 ORDER BY timestamp_epoch_s ASC",
            )
            .map_err(|e| format!("failed to prepare breadcrumb query: {e}"))?;

        let rows = stmt
            .query_map([issue_id], |row| {
                let phase_str: String = row.get(4)?;
                let decision_json: String = row.get(5)?;
                let files_json: String = row.get(8)?;

                Ok(BreadcrumbRow {
                    breadcrumb_id: row.get(0)?,
                    session_id: row.get(1)?,
                    issue_id: row.get(2)?,
                    attempt_number: row.get(3)?,
                    phase_reached: phase_str,
                    decision_json,
                    patch_description: row.get(6)?,
                    error_output: row.get(7)?,
                    files_modified_json: files_json,
                    fingerprint_before: row.get(9)?,
                    fingerprint_after: row.get(10)?,
                    timestamp_epoch_s: row.get(11)?,
                })
            })
            .map_err(|e| format!("failed to query breadcrumbs: {e}"))?;

        let mut breadcrumbs = Vec::new();
        for row in rows {
            let row = row.map_err(|e| format!("failed to read breadcrumb row: {e}"))?;
            breadcrumbs.push(row.into_breadcrumb()?);
        }
        Ok(breadcrumbs)
    }

    /// Count failed attempts for an issue.
    pub fn count_failed_attempts(&self, issue_id: &str) -> Result<u32, String> {
        let conn = self.lock()?;
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM evolution_breadcrumbs
                 WHERE issue_id = ?1 AND decision_json LIKE '%rollback%'",
                [issue_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("failed to count failed attempts: {e}"))?;
        Ok(count as u32)
    }

    /// Insert or update a session summary.
    pub fn upsert_session(&self, s: &EvolutionSessionSummary) -> Result<(), String> {
        let conn = self.lock()?;
        let trigger_detail = serde_json::to_string(&s.trigger)
            .map_err(|e| format!("failed to serialize trigger: {e}"))?;

        conn.execute(
            "INSERT OR REPLACE INTO evolution_sessions
             (session_id, mode, trigger_kind, trigger_detail,
              started_at_epoch_s, completed_at_epoch_s,
              issues_detected, patches_applied, patches_rolled_back, patches_skipped)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                s.session_id,
                s.mode.as_str(),
                s.trigger.kind_str(),
                trigger_detail,
                s.started_at_epoch_s,
                s.completed_at_epoch_s,
                s.issues_detected,
                s.patches_applied,
                s.patches_rolled_back,
                s.patches_skipped,
            ],
        )
        .map_err(|e| format!("failed to upsert session: {e}"))?;
        Ok(())
    }

    /// Get the most recent completed session.
    pub fn latest_session(&self) -> Result<Option<EvolutionSessionSummary>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT session_id, mode, trigger_kind, trigger_detail,
                        started_at_epoch_s, completed_at_epoch_s,
                        issues_detected, patches_applied, patches_rolled_back, patches_skipped
                 FROM evolution_sessions
                 ORDER BY started_at_epoch_s DESC
                 LIMIT 1",
            )
            .map_err(|e| format!("failed to prepare session query: {e}"))?;

        let mut rows = stmt
            .query_map([], |row| {
                Ok(SessionRow {
                    session_id: row.get(0)?,
                    mode: row.get(1)?,
                    trigger_kind: row.get(2)?,
                    trigger_detail: row.get(3)?,
                    started_at_epoch_s: row.get(4)?,
                    completed_at_epoch_s: row.get(5)?,
                    issues_detected: row.get(6)?,
                    patches_applied: row.get(7)?,
                    patches_rolled_back: row.get(8)?,
                    patches_skipped: row.get(9)?,
                })
            })
            .map_err(|e| format!("failed to query sessions: {e}"))?;

        match rows.next() {
            Some(Ok(row)) => Ok(Some(row.into_session()?)),
            Some(Err(e)) => Err(format!("failed to read session row: {e}")),
            None => Ok(None),
        }
    }

    /// Resolve the default database path.
    pub fn default_db_path() -> PathBuf {
        let base = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join(".loongclaw").join("evolution.db")
    }
}

// -- Internal row types for deserialization --

struct BreadcrumbRow {
    breadcrumb_id: String,
    session_id: String,
    issue_id: String,
    attempt_number: u32,
    phase_reached: String,
    decision_json: String,
    patch_description: String,
    error_output: Option<String>,
    files_modified_json: String,
    fingerprint_before: String,
    fingerprint_after: Option<String>,
    timestamp_epoch_s: i64,
}

impl BreadcrumbRow {
    fn into_breadcrumb(self) -> Result<EvolutionBreadcrumb, String> {
        let phase_reached = match self.phase_reached.as_str() {
            "detect" => EvolutionPhase::Detect,
            "diagnose" => EvolutionPhase::Diagnose,
            "snapshot" => EvolutionPhase::Snapshot,
            "patch" => EvolutionPhase::Patch,
            "verify" => EvolutionPhase::Verify,
            "decide" => EvolutionPhase::Decide,
            other => {
                return Err(format!("unknown evolution phase: {other}"));
            }
        };

        let decision: EvolutionDecision = serde_json::from_str(&self.decision_json)
            .map_err(|e| format!("failed to deserialize decision: {e}"))?;

        let files_modified: Vec<String> = serde_json::from_str(&self.files_modified_json)
            .map_err(|e| format!("failed to deserialize files_modified: {e}"))?;

        Ok(EvolutionBreadcrumb {
            breadcrumb_id: self.breadcrumb_id,
            session_id: self.session_id,
            issue_id: self.issue_id,
            attempt_number: self.attempt_number,
            phase_reached,
            decision,
            patch_description: self.patch_description,
            error_output: self.error_output,
            files_modified,
            fingerprint_before: self.fingerprint_before,
            fingerprint_after: self.fingerprint_after,
            timestamp_epoch_s: self.timestamp_epoch_s,
        })
    }
}

struct SessionRow {
    session_id: String,
    mode: String,
    trigger_kind: String,
    trigger_detail: Option<String>,
    started_at_epoch_s: i64,
    completed_at_epoch_s: Option<i64>,
    issues_detected: u32,
    patches_applied: u32,
    patches_rolled_back: u32,
    patches_skipped: u32,
}

impl SessionRow {
    fn into_session(self) -> Result<EvolutionSessionSummary, String> {
        let mode = match self.mode.as_str() {
            "scan" => EvolutionMode::Scan,
            "fix" => EvolutionMode::Fix,
            "improve" => EvolutionMode::Improve,
            "full" => EvolutionMode::Full,
            other => return Err(format!("unknown evolution mode: {other}")),
        };

        let trigger = if let Some(detail) = &self.trigger_detail {
            serde_json::from_str(detail)
                .map_err(|e| format!("failed to deserialize trigger: {e}"))?
        } else {
            match self.trigger_kind.as_str() {
                "manual" => EvolutionTrigger::Manual,
                _ => EvolutionTrigger::Manual,
            }
        };

        Ok(EvolutionSessionSummary {
            session_id: self.session_id,
            mode,
            trigger,
            started_at_epoch_s: self.started_at_epoch_s,
            completed_at_epoch_s: self.completed_at_epoch_s,
            issues_detected: self.issues_detected,
            patches_applied: self.patches_applied,
            patches_rolled_back: self.patches_rolled_back,
            patches_skipped: self.patches_skipped,
        })
    }
}

#[cfg(test)]
mod tests {
    use loongclaw_contracts::{EvolutionDecision, EvolutionPhase};

    use super::*;

    #[test]
    fn open_in_memory_creates_schema() {
        let store =
            EvolutionBreadcrumbStore::open_in_memory().expect("should open in-memory store");
        // Verify tables exist by running a query.
        let result = store.latest_session();
        assert!(result.is_ok());
        assert!(result.expect("query should succeed").is_none());
    }

    #[test]
    fn insert_and_query_breadcrumb() {
        let store =
            EvolutionBreadcrumbStore::open_in_memory().expect("should open in-memory store");

        // Insert a session first (foreign key).
        let session = EvolutionSessionSummary {
            session_id: "ev-test-001".to_owned(),
            mode: EvolutionMode::Fix,
            trigger: EvolutionTrigger::Manual,
            started_at_epoch_s: 1000,
            completed_at_epoch_s: None,
            issues_detected: 1,
            patches_applied: 0,
            patches_rolled_back: 0,
            patches_skipped: 0,
        };
        store.upsert_session(&session).expect("session insert");

        let breadcrumb = EvolutionBreadcrumb {
            breadcrumb_id: "bc-001".to_owned(),
            session_id: "ev-test-001".to_owned(),
            issue_id: "issue-abc".to_owned(),
            attempt_number: 1,
            phase_reached: EvolutionPhase::Verify,
            decision: EvolutionDecision::Rollback {
                reason: "tests failed".to_owned(),
                breadcrumb_id: "bc-001".to_owned(),
            },
            patch_description: "clippy autofix".to_owned(),
            error_output: Some("assertion failed".to_owned()),
            files_modified: vec!["crates/app/src/tools/mod.rs".to_owned()],
            fingerprint_before: "abc123".to_owned(),
            fingerprint_after: None,
            timestamp_epoch_s: 1001,
        };
        store
            .insert_breadcrumb(&breadcrumb)
            .expect("breadcrumb insert");

        let results = store.query_by_issue("issue-abc").expect("breadcrumb query");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].breadcrumb_id, "bc-001");
        assert_eq!(results[0].attempt_number, 1);
        assert!(matches!(
            results[0].decision,
            EvolutionDecision::Rollback { .. }
        ));
    }

    #[test]
    fn count_failed_attempts_counts_rollbacks() {
        let store =
            EvolutionBreadcrumbStore::open_in_memory().expect("should open in-memory store");

        let session = EvolutionSessionSummary {
            session_id: "ev-test-002".to_owned(),
            mode: EvolutionMode::Fix,
            trigger: EvolutionTrigger::Manual,
            started_at_epoch_s: 2000,
            completed_at_epoch_s: None,
            issues_detected: 1,
            patches_applied: 0,
            patches_rolled_back: 2,
            patches_skipped: 0,
        };
        store.upsert_session(&session).expect("session insert");

        for i in 1..=3 {
            let decision = if i <= 2 {
                EvolutionDecision::Rollback {
                    reason: "failed".to_owned(),
                    breadcrumb_id: format!("bc-{i}"),
                }
            } else {
                EvolutionDecision::Keep {
                    commit_sha: "abc".to_owned(),
                }
            };
            store
                .insert_breadcrumb(&EvolutionBreadcrumb {
                    breadcrumb_id: format!("bc-count-{i}"),
                    session_id: "ev-test-002".to_owned(),
                    issue_id: "issue-xyz".to_owned(),
                    attempt_number: i,
                    phase_reached: EvolutionPhase::Decide,
                    decision,
                    patch_description: format!("attempt {i}"),
                    error_output: None,
                    files_modified: vec![],
                    fingerprint_before: "fp".to_owned(),
                    fingerprint_after: None,
                    timestamp_epoch_s: 2000 + i64::from(i),
                })
                .expect("breadcrumb insert");
        }

        let count = store
            .count_failed_attempts("issue-xyz")
            .expect("count query");
        assert_eq!(count, 2);
    }

    #[test]
    fn upsert_and_latest_session() {
        let store =
            EvolutionBreadcrumbStore::open_in_memory().expect("should open in-memory store");

        let session1 = EvolutionSessionSummary {
            session_id: "ev-old".to_owned(),
            mode: EvolutionMode::Scan,
            trigger: EvolutionTrigger::Manual,
            started_at_epoch_s: 1000,
            completed_at_epoch_s: Some(1010),
            issues_detected: 3,
            patches_applied: 0,
            patches_rolled_back: 0,
            patches_skipped: 0,
        };
        store.upsert_session(&session1).expect("session 1 insert");

        let session2 = EvolutionSessionSummary {
            session_id: "ev-new".to_owned(),
            mode: EvolutionMode::Full,
            trigger: EvolutionTrigger::Scheduled {
                schedule_id: "cron-1".to_owned(),
            },
            started_at_epoch_s: 2000,
            completed_at_epoch_s: Some(2020),
            issues_detected: 5,
            patches_applied: 2,
            patches_rolled_back: 1,
            patches_skipped: 2,
        };
        store.upsert_session(&session2).expect("session 2 insert");

        let latest = store.latest_session().expect("latest query");
        assert!(latest.is_some());
        let latest = latest.expect("should have session");
        assert_eq!(latest.session_id, "ev-new");
        assert_eq!(latest.patches_applied, 2);
    }
}
