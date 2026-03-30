use loongclaw_contracts::{ToolCoreOutcome, ToolCoreRequest};
#[cfg(feature = "tool-shell")]
use serde_json::{Value, json};
#[cfg(feature = "tool-shell")]
use std::process::Stdio;
#[cfg(feature = "tool-shell")]
use std::time::Duration;
#[cfg(feature = "tool-shell")]
use std::{future::Future, path::PathBuf, thread};
#[cfg(feature = "tool-shell")]
use tokio::{io::AsyncReadExt, process::Command};

pub(super) fn execute_shell_tool_with_config(
    request: ToolCoreRequest,
    config: &super::runtime_config::ToolRuntimeConfig,
) -> Result<ToolCoreOutcome, String> {
    #[cfg(not(feature = "tool-shell"))]
    {
        let _ = (request, config);
        return Err(
            "shell tool is disabled in this build (enable feature `tool-shell`)".to_owned(),
        );
    }

    #[cfg(feature = "tool-shell")]
    {
        let payload = request
            .payload
            .as_object()
            .ok_or_else(|| "shell.exec payload must be an object".to_owned())?;
        let raw_command = payload
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "shell.exec requires payload.command".to_owned())?;
        let mut explicit_args = payload
            .get("args")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Validate the original command name against shell policy BEFORE any
        // rewriting (sh -c wrapping, whitespace splitting, etc.).
        let normalized_command =
            crate::tools::shell_policy_ext::validate_shell_command_name(raw_command)?;
        let basename = normalized_command.as_str();

        // Local models often send the full command line as a single string
        // (e.g. "curl -s http://...") instead of splitting into command + args.
        // Auto-split when the command field contains whitespace and no explicit
        // args were provided.
        //
        // When the command contains shell operators (pipes, redirects, etc.),
        // wrap the entire command line in `sh -c` so they are interpreted
        // correctly instead of being passed as literal arguments.
        let (command, args) =
            if raw_command.contains(char::is_whitespace) && explicit_args.is_empty() {
                if contains_shell_operators(raw_command) {
                    (
                        "sh".to_owned(),
                        vec!["-c".to_owned(), raw_command.to_owned()],
                    )
                } else {
                    let mut parts = raw_command.split_whitespace();
                    let cmd = parts.next().unwrap_or(raw_command);
                    let mut split_args: Vec<String> = parts.map(str::to_owned).collect();
                    split_args.append(&mut explicit_args);
                    (cmd.to_owned(), split_args)
                }
            } else if !explicit_args.is_empty()
                && explicit_args.iter().any(|a| {
                    a.contains('|') || a.contains('>') || a.contains('<') || a == "&&" || a == "||"
                })
            {
                // The model split the command and args but included shell
                // operators in the args list — reassemble and run via sh.
                let mut full = raw_command.to_owned();
                for arg in &explicit_args {
                    full.push(' ');
                    full.push_str(arg);
                }
                ("sh".to_owned(), vec!["-c".to_owned(), full])
            } else {
                (raw_command.to_owned(), explicit_args)
            };
        let command = command.as_str();
        let cwd = payload
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let timeout_ms = parse_shell_timeout_ms(payload)?;

        if config.shell_deny.contains(basename) {
            return Err(format!(
                "policy_denied: shell command `{basename}` is blocked by shell policy"
            ));
        }

        let explicitly_allowed = config.shell_allow.contains(basename);
        let default_allows = matches!(
            config.shell_default_mode,
            crate::tools::shell_policy_ext::ShellPolicyDefault::Allow
        );
        if !explicitly_allowed && !default_allows {
            return Err(format!(
                "policy_denied: shell command `{basename}` is not in the allow list (default-deny policy)"
            ));
        }

        let output = run_shell_async(run_shell_command_with_timeout(
            command,
            &args,
            cwd.as_path(),
            timeout_ms,
        ))??;

        Ok(ToolCoreOutcome {
            status: if output.status.success() {
                "ok".to_owned()
            } else {
                "failed".to_owned()
            },
            payload: json!({
                "adapter": "core-tools",
                "tool_name": request.tool_name,
                "command": command,
                "args": args,
                "cwd": cwd.display().to_string(),
                "exit_code": output.status.code(),
                "stdout": String::from_utf8_lossy(&output.stdout).trim().to_owned(),
                "stderr": String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            }),
        })
    }
}

/// Returns true if the command string contains shell operators that require
/// interpretation by a shell (pipes, redirects, logical operators, subshells).
#[cfg(feature = "tool-shell")]
fn contains_shell_operators(cmd: &str) -> bool {
    // Check for common shell metacharacters. We look for standalone operators
    // rather than substrings inside quoted text, but since local models rarely
    // quote properly this is a pragmatic heuristic.
    cmd.contains('|')
        || cmd.contains('>')
        || cmd.contains('<')
        || cmd.contains("&&")
        || cmd.contains("||")
        || cmd.contains(';')
        || cmd.contains('$')
        || cmd.contains('`')
}

#[cfg(feature = "tool-shell")]
const SHELL_EXEC_DEFAULT_TIMEOUT_MS: u64 = 30_000;
#[cfg(feature = "tool-shell")]
const SHELL_EXEC_MAX_TIMEOUT_MS: u64 = 600_000;
#[cfg(feature = "tool-shell")]
const SHELL_OUTPUT_CAP_BYTES: usize = 1_048_576;

#[cfg(feature = "tool-shell")]
fn parse_shell_timeout_ms(payload: &serde_json::Map<String, Value>) -> Result<u64, String> {
    let timeout_ms = match payload.get("timeout_ms") {
        Some(timeout_ms) => timeout_ms
            .as_u64()
            .ok_or_else(|| "shell.exec payload.timeout_ms must be an integer".to_owned())?,
        None => SHELL_EXEC_DEFAULT_TIMEOUT_MS,
    };

    Ok(timeout_ms.clamp(1_000, SHELL_EXEC_MAX_TIMEOUT_MS))
}

#[cfg(feature = "tool-shell")]
fn run_shell_async<F>(future: F) -> Result<F::Output, String>
where
    F: Future + Send,
    F::Output: Send,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            Ok(tokio::task::block_in_place(|| handle.block_on(future)))
        }
        Ok(_) => thread::scope(|scope| {
            scope
                .spawn(|| {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|error| {
                            format!("failed to create tokio runtime for shell tool: {error}")
                        })?;
                    Ok(rt.block_on(future))
                })
                .join()
                .map_err(|_panic| "shell tool async worker thread panicked".to_owned())?
        }),
        Err(_) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| {
                    format!("failed to create tokio runtime for shell tool: {error}")
                })?;
            Ok(rt.block_on(future))
        }
    }
}

#[cfg(feature = "tool-shell")]
async fn read_capped<R>(mut reader: R, cap: usize, stream_name: &str) -> Result<Vec<u8>, String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8_192];

    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| format!("shell command {stream_name} read failed: {error}"))?;
        if read == 0 {
            break;
        }

        let remaining = cap.saturating_sub(output.len());
        if remaining > 0 {
            let to_copy = remaining.min(read);
            output.extend(buffer.iter().take(to_copy).copied());
        }
    }

    Ok(output)
}

#[cfg(feature = "tool-shell")]
async fn run_shell_command_with_timeout(
    command: &str,
    args: &[String],
    cwd: &std::path::Path,
    timeout_ms: u64,
) -> Result<std::process::Output, String> {
    let mut child = Command::new(command)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("shell command spawn failed: {error}"))?;

    let duration = Duration::from_millis(timeout_ms.max(1));
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "shell command stdout pipe missing".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "shell command stderr pipe missing".to_owned())?;

    let stdout_task =
        tokio::spawn(async move { read_capped(stdout, SHELL_OUTPUT_CAP_BYTES, "stdout").await });
    let stderr_task =
        tokio::spawn(async move { read_capped(stderr, SHELL_OUTPUT_CAP_BYTES, "stderr").await });

    match tokio::time::timeout(duration, child.wait()).await {
        Ok(Ok(status)) => {
            let (stdout_result, stderr_result) = tokio::join!(stdout_task, stderr_task);
            let stdout = stdout_result.map_err(|join_error| {
                format!("shell command stdout reader panicked: {join_error}")
            })??;
            let stderr = stderr_result.map_err(|join_error| {
                format!("shell command stderr reader panicked: {join_error}")
            })??;

            Ok(std::process::Output {
                status,
                stdout,
                stderr,
            })
        }
        Ok(Err(error)) => {
            stdout_task.abort();
            stderr_task.abort();
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = tokio::join!(stdout_task, stderr_task);
            Err(format!("shell command wait failed: {error}"))
        }
        Err(_) => {
            stdout_task.abort();
            stderr_task.abort();
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = tokio::join!(stdout_task, stderr_task);
            Err(format!("shell command timed out after {timeout_ms}ms"))
        }
    }
}
