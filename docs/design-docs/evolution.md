# Self-Evolution System

LoongClaw's self-evolution system allows the agent to detect issues in its own codebase, apply fixes, verify them, and automatically rollback failed attempts — with breadcrumbs stored in SQLite so it never retries the same bad fix.

## Design Principles

1. **Architecture guard enforced** — the evolution engine respects `ArchitectureBoundaryPolicy`. Kernel core files (kernel.rs, contracts.rs, policy.rs, etc.) are immutable. Only mutable extension zones (daemon, tools, channels, docs, examples) can be modified.

2. **Git snapshot before every mutation** — every patch is preceded by a git commit. On failure, `git reset --hard` restores the exact prior state. No silent corruption.

3. **Breadcrumb deduplication** — failed attempts are recorded in SQLite with issue ID, patch description, and error output. The engine checks breadcrumbs before attempting a fix and skips issues that have exceeded `max_retries_per_issue`.

4. **Pure engine, injected I/O** — `EvolutionEngine` (kernel crate) contains only FSM logic. All side effects (shell commands, file I/O, breadcrumb storage) go through the `EvolutionExecutor` trait, making the engine fully testable with mocks.

## Architecture

```text
contracts (EvolutionMode, EvolutionIssue, EvolutionBreadcrumb, EvolutionConfig, ...)
  └── kernel (EvolutionEngine + EvolutionExecutor trait)
       └── app (AppEvolutionExecutor + EvolutionBreadcrumbStore + watch daemon)
            └── daemon (evolve_cli: CLI subcommand)
```

## Evolution Cycle FSM

```text
DETECT ─> DIAGNOSE ─> SNAPSHOT ─> PATCH ─> VERIFY ─> DECIDE
                                                       ├─ PASS ─> Keep (commit stays)
                                                       └─ FAIL ─> Rollback + breadcrumb
```

| Phase | What happens |
|-------|-------------|
| DETECT | Run `cargo test`, `cargo clippy`. Parse output for failures/warnings. |
| DIAGNOSE | Map issue kind to a patch strategy (clippy autofix, compiler fix, etc.). |
| SNAPSHOT | `git add -A && git commit` + record commit SHA. |
| PATCH | Apply fix (e.g. `cargo clippy --fix`). Guard modified files against architecture policy. |
| VERIFY | Run all verify commands (tests, clippy). |
| DECIDE | Pass: commit the fix. Fail: `git reset --hard <snapshot_sha>` + store breadcrumb. |

## Modes

| Mode | Behavior |
|------|----------|
| `scan` | Detect only. No patches applied. |
| `fix` | Reactive: fix test failures and compilation errors. |
| `improve` | Proactive: apply clippy autofixes, detect missing tests. |
| `full` | Both fix and improve. |

## CLI

```bash
# One-shot
loongclaw evolve --mode scan              # detect issues only
loongclaw evolve --mode fix               # fix failures
loongclaw evolve --mode improve           # proactive improvements
loongclaw evolve --mode full --dry-run    # full cycle, rollback all patches
loongclaw evolve --json                   # structured JSON output

# Watch daemon
loongclaw evolve --watch 300              # scan every 5 minutes
loongclaw evolve --watch 300 --force-interval 3600  # + force full cycle hourly
```

## Watch Daemon

The `--watch` flag starts a long-running daemon with two timers:

1. **Scan interval** (`--watch <secs>`): light scan, only evolves if issues found.
2. **Force interval** (`--force-interval <secs>`): unconditional full evolution cycle. Default 3600s. Set to 0 to disable.

Uses `tokio::select!` with `with_graceful_shutdown()` for clean Ctrl-C/SIGTERM handling.

## Configuration

`EvolutionConfig` (defaults):

| Field | Default | Description |
|-------|---------|-------------|
| `max_retries_per_issue` | 3 | Max failed attempts before skipping an issue |
| `max_mutations_per_session` | 10 | Max patches applied per session |
| `cooldown_seconds` | 300 | Min time between sessions |
| `auto_commit` | true | Commit fixes automatically |
| `dry_run` | false | Apply patches then rollback (for testing) |
| `verify_commands` | cargo test, cargo clippy | Commands run in VERIFY phase |

## SQLite Schema

Stored at `~/.loongclaw/evolution.db`, separate from conversation memory.

**Tables:**
- `evolution_sessions` — session ID, mode, trigger, timestamps, counters
- `evolution_breadcrumbs` — issue ID, attempt number, phase reached, decision, patch description, error output, fingerprints
- `evolution_schema_version` — schema migration tracking

## Safety Rails

- Architecture guard prevents mutation of kernel core files
- Git snapshot before every mutation enables clean rollback
- Breadcrumb deduplication prevents infinite retry loops
- Max mutations per session caps blast radius
- Cooldown prevents rapid-fire sessions
- Dry-run mode for safe experimentation

## Key Files

| File | Purpose |
|------|---------|
| `crates/contracts/src/evolution_types.rs` | All shared types |
| `crates/kernel/src/evolution.rs` | Engine FSM + executor trait |
| `crates/app/src/memory/evolution_store.rs` | SQLite breadcrumb store |
| `crates/app/src/evolution/mod.rs` | AppEvolutionExecutor |
| `crates/app/src/evolution/watch.rs` | Watch-mode daemon |
| `crates/daemon/src/evolve_cli.rs` | CLI subcommand |

## Future Work

- **AI-driven code generation**: wire the PATCH phase to a provider (e.g. GLM, Anthropic) for strategies beyond clippy autofix (compiler error fixes, missing tests, refactors).
- **Runtime error trigger**: hook into the conversation turn engine to auto-trigger evolution on runtime errors.
- **Post-spec-failure trigger**: auto-trigger after `loongclaw run-spec` failures.
- **Heartbeat and stale detection**: write heartbeat file in watch mode so other processes can detect if daemon is alive.
