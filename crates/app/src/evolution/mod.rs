pub mod watch;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use loongclaw_contracts::{EvolutionBreadcrumb, EvolutionSessionSummary, ShellOutput};
use loongclaw_kernel::EvolutionExecutor;

use crate::memory::evolution_store::EvolutionBreadcrumbStore;

/// Concrete implementation of `EvolutionExecutor` that wires to loongclaw's
/// existing tool infrastructure (shell.exec, file.read, file.write) and the
/// SQLite breadcrumb store.
pub struct AppEvolutionExecutor {
    store: Arc<EvolutionBreadcrumbStore>,
    workspace_root: PathBuf,
    id_counter: std::sync::atomic::AtomicU64,
}

impl AppEvolutionExecutor {
    pub fn new(store: Arc<EvolutionBreadcrumbStore>, workspace_root: PathBuf) -> Self {
        Self {
            store,
            workspace_root,
            id_counter: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

#[async_trait::async_trait]
impl EvolutionExecutor for AppEvolutionExecutor {
    async fn shell_exec(&self, command: &str, cwd: Option<&str>) -> Result<ShellOutput, String> {
        let work_dir = cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| self.workspace_root.clone());

        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&work_dir)
            .output()
            .await
            .map_err(|e| format!("shell_exec failed: {e}"))?;

        Ok(ShellOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }

    async fn read_file(&self, path: &str) -> Result<String, String> {
        let full_path = self.workspace_root.join(path);
        tokio::fs::read_to_string(&full_path)
            .await
            .map_err(|e| format!("read_file({path}) failed: {e}"))
    }

    async fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        let full_path = self.workspace_root.join(path);
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("create_dir_all failed: {e}"))?;
        }
        tokio::fs::write(&full_path, content)
            .await
            .map_err(|e| format!("write_file({path}) failed: {e}"))
    }

    async fn store_breadcrumb(&self, breadcrumb: &EvolutionBreadcrumb) -> Result<(), String> {
        self.store.insert_breadcrumb(breadcrumb)
    }

    async fn query_breadcrumbs(&self, issue_id: &str) -> Result<Vec<EvolutionBreadcrumb>, String> {
        self.store.query_by_issue(issue_id)
    }

    async fn latest_session(&self) -> Result<Option<EvolutionSessionSummary>, String> {
        self.store.latest_session()
    }

    async fn save_session(&self, session: &EvolutionSessionSummary) -> Result<(), String> {
        self.store.upsert_session(session)
    }

    fn now_epoch_s(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn generate_id(&self, prefix: &str) -> String {
        let counter = self
            .id_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ts = self.now_epoch_s();
        format!("{prefix}-{ts}-{counter:04}")
    }
}
