use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use loongclaw_contracts::{EvolutionConfig, EvolutionMode, EvolutionTrigger};
use loongclaw_kernel::{ArchitectureBoundaryPolicy, EvolutionEngine};

use crate::memory::evolution_store::EvolutionBreadcrumbStore;

use super::AppEvolutionExecutor;

/// Options for the watch-mode daemon.
pub struct EvolveWatchOptions {
    pub workspace_root: PathBuf,
    pub db_path: PathBuf,
    pub scan_interval_secs: u64,
    pub force_interval_secs: u64,
    pub mode: EvolutionMode,
    pub config: EvolutionConfig,
    pub json: bool,
}

/// Run the evolution watch-mode daemon.
///
/// This enters an infinite loop that periodically scans for issues and
/// auto-fixes them. It uses `tokio::select!` to race the scan interval,
/// the force interval, and a shutdown signal.
#[allow(clippy::print_stderr)]
pub async fn run_evolve_watch(options: EvolveWatchOptions) -> Result<(), String> {
    let store = Arc::new(
        EvolutionBreadcrumbStore::open(&options.db_path)
            .map_err(|e| format!("failed to open evolution store: {e}"))?,
    );
    let executor = AppEvolutionExecutor::new(store, options.workspace_root.clone());
    let engine = EvolutionEngine::new(
        ArchitectureBoundaryPolicy::default(),
        options.config.clone(),
    );

    let scan_interval = Duration::from_secs(options.scan_interval_secs);
    let force_interval = Duration::from_secs(options.force_interval_secs);
    let force_enabled = options.force_interval_secs > 0;

    eprintln!(
        "[evolve-watch] started (scan={}s, force={}s, mode={})",
        options.scan_interval_secs,
        options.force_interval_secs,
        options.mode.as_str()
    );

    let mut scan_ticker = tokio::time::interval(scan_interval);
    let mut force_ticker = tokio::time::interval(if force_enabled {
        force_interval
    } else {
        Duration::from_secs(u64::MAX / 2)
    });

    // First tick fires immediately — skip it so we don't run on startup.
    scan_ticker.tick().await;
    force_ticker.tick().await;

    loop {
        tokio::select! {
            _ = scan_ticker.tick() => {
                eprintln!("[evolve-watch] scan tick: running {} scan...", options.mode.as_str());
                match engine.run_session(&executor, options.mode, EvolutionTrigger::Scheduled {
                    schedule_id: "watch-scan".to_owned(),
                }).await {
                    Ok(summary) => {
                        eprintln!(
                            "[evolve-watch] session {} complete: {} detected, {} applied, {} rolled back, {} skipped",
                            summary.session_id,
                            summary.issues_detected,
                            summary.patches_applied,
                            summary.patches_rolled_back,
                            summary.patches_skipped,
                        );
                        if options.json
                            && let Ok(json) = serde_json::to_string(&summary)
                        {
                            eprintln!("{json}");
                        }
                    }
                    Err(e) => {
                        eprintln!("[evolve-watch] scan error: {e}");
                    }
                }
            }
            _ = force_ticker.tick(), if force_enabled => {
                eprintln!("[evolve-watch] force tick: running full evolution cycle...");
                match engine.run_session(&executor, EvolutionMode::Full, EvolutionTrigger::Scheduled {
                    schedule_id: "watch-force".to_owned(),
                }).await {
                    Ok(summary) => {
                        eprintln!(
                            "[evolve-watch] force session {} complete: {} detected, {} applied, {} rolled back, {} skipped",
                            summary.session_id,
                            summary.issues_detected,
                            summary.patches_applied,
                            summary.patches_rolled_back,
                            summary.patches_skipped,
                        );
                    }
                    Err(e) => {
                        eprintln!("[evolve-watch] force error: {e}");
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\n[evolve-watch] received Ctrl-C, shutting down gracefully...");
                break;
            }
        }
    }

    eprintln!("[evolve-watch] stopped");
    Ok(())
}
