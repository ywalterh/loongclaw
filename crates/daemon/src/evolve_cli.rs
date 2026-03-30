use std::path::PathBuf;
use std::sync::Arc;

use crate::CliResult;
use crate::kernel::{
    ArchitectureBoundaryPolicy, EvolutionConfig, EvolutionEngine, EvolutionMode, EvolutionTrigger,
};
use crate::mvp::evolution::AppEvolutionExecutor;
use crate::mvp::evolution::watch::{EvolveWatchOptions, run_evolve_watch};
use crate::mvp::memory::evolution_store::EvolutionBreadcrumbStore;
use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum EvolveMode {
    Scan,
    Fix,
    Improve,
    Full,
}

impl EvolveMode {
    fn to_evolution_mode(self) -> EvolutionMode {
        match self {
            Self::Scan => EvolutionMode::Scan,
            Self::Fix => EvolutionMode::Fix,
            Self::Improve => EvolutionMode::Improve,
            Self::Full => EvolutionMode::Full,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EvolveCommandOptions {
    pub config: Option<String>,
    pub mode: EvolveMode,
    pub json: bool,
    pub dry_run: bool,
    pub max_mutations: Option<u32>,
    pub watch: Option<u64>,
    pub force_interval: Option<u64>,
}

pub async fn run_evolve_cli(options: EvolveCommandOptions) -> CliResult<()> {
    let workspace_root = resolve_workspace_root()?;
    let db_path = EvolutionBreadcrumbStore::default_db_path();

    let mut evolution_config = EvolutionConfig::default();
    if options.dry_run {
        evolution_config.dry_run = true;
    }
    if let Some(max) = options.max_mutations {
        evolution_config.max_mutations_per_session = max;
    }

    let mode = options.mode.to_evolution_mode();

    // Watch mode: long-running daemon
    if let Some(scan_interval) = options.watch {
        return run_evolve_watch(EvolveWatchOptions {
            workspace_root,
            db_path,
            scan_interval_secs: scan_interval,
            force_interval_secs: options.force_interval.unwrap_or(3600),
            mode,
            config: evolution_config,
            json: options.json,
        })
        .await;
    }

    // One-shot mode
    let store = Arc::new(
        EvolutionBreadcrumbStore::open(&db_path)
            .map_err(|e| format!("failed to open evolution store: {e}"))?,
    );
    let executor = AppEvolutionExecutor::new(store, workspace_root);
    let engine = EvolutionEngine::new(ArchitectureBoundaryPolicy::default(), evolution_config);

    // Check cooldown
    if !engine.check_cooldown(&executor).await? {
        let msg = "evolution cooldown not elapsed; skipping";
        if options.json {
            println!("{{\"status\": \"cooldown\", \"message\": \"{msg}\"}}");
        } else {
            eprintln!("{msg}");
        }
        return Ok(());
    }

    let summary = engine
        .run_session(&executor, mode, EvolutionTrigger::Manual)
        .await?;

    if options.json {
        let json = serde_json::to_string_pretty(&summary)
            .map_err(|e| format!("failed to serialize summary: {e}"))?;
        println!("{json}");
    } else {
        println!("Evolution session: {}", summary.session_id);
        println!("  Mode:            {}", summary.mode.as_str());
        println!("  Issues detected: {}", summary.issues_detected);
        println!("  Patches applied: {}", summary.patches_applied);
        println!("  Rolled back:     {}", summary.patches_rolled_back);
        println!("  Skipped:         {}", summary.patches_skipped);
        if let Some(completed) = summary.completed_at_epoch_s {
            let duration = completed - summary.started_at_epoch_s;
            println!("  Duration:        {}s", duration);
        }
    }

    Ok(())
}

fn resolve_workspace_root() -> Result<PathBuf, String> {
    // Try to find the workspace root by looking for Cargo.toml with [workspace].
    let cwd =
        std::env::current_dir().map_err(|e| format!("failed to get current directory: {e}"))?;

    let mut dir = cwd.as_path();
    loop {
        let cargo_toml = dir.join("Cargo.toml");
        if cargo_toml.exists()
            && let Ok(content) = std::fs::read_to_string(&cargo_toml)
            && content.contains("[workspace]")
        {
            return Ok(dir.to_path_buf());
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }

    // Fallback to current directory.
    Ok(cwd)
}
