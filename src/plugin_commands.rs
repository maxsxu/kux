use anyhow::Result;
use kux::{
    app::App,
    plugin_process::{PluginProcessManager, PluginProcessResult, run_foreground},
    plugins::PluginInvocation,
    terminal::TerminalGuard,
};

pub(super) async fn execute(
    app: &mut App,
    terminal: &mut TerminalGuard,
    processes: &mut PluginProcessManager,
    invocation: PluginInvocation,
) -> Result<()> {
    if app.read_only && invocation.dangerous {
        app.show_transient_notice("This plugin is disabled in read-only mode");
    } else if invocation.background {
        let description = invocation.description.clone();
        match processes.start(invocation) {
            Ok(_) => app.show_transient_notice(format!("Started {description}")),
            Err(error) => app.show_transient_notice(format!("Plugin failed: {error}")),
        }
    } else {
        terminal.suspend()?;
        let result = run_foreground(&invocation).await;
        terminal.resume()?;
        match result {
            Ok(status) if status.success() => {
                app.show_transient_notice(format!("{} completed", invocation.description));
            }
            Ok(status) => app
                .show_transient_notice(format!("{} exited with {status}", invocation.description)),
            Err(error) => app.show_transient_notice(format!("Plugin failed: {error}")),
        }
    }
    Ok(())
}

pub(super) fn poll(app: &mut App, processes: &mut PluginProcessManager) {
    for result in processes.poll() {
        app.show_transient_notice(app.connection.redact(&completion_notice(&result)));
    }
}

fn completion_notice(result: &PluginProcessResult) -> String {
    if result.cancelled {
        return format!("{} cancelled", result.description);
    }
    let detail = |output: &str| -> Option<String> {
        output
            .lines()
            .find(|line| !line.trim().is_empty())
            .map(|line| {
                line.chars()
                    .filter(|character| !character.is_control())
                    .take(512)
                    .collect()
            })
    };
    if result.success {
        if result.overwrite_output
            && let Some(output) = detail(&result.stdout)
        {
            return output;
        }
        return format!("{} completed", result.description);
    }
    let reason = result
        .error
        .as_deref()
        .and_then(detail)
        .or_else(|| detail(&result.stderr))
        .unwrap_or_else(|| {
            result.exit_code.map_or_else(
                || "process terminated".to_owned(),
                |code| format!("exit code {code}"),
            )
        });
    format!("{} failed: {reason}", result.description)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result() -> PluginProcessResult {
        PluginProcessResult {
            id: 1,
            name: "example".to_owned(),
            description: "Example".to_owned(),
            success: true,
            exit_code: Some(0),
            cancelled: false,
            stdout: "\nFirst line\nSecond line".to_owned(),
            stderr: String::new(),
            truncated: false,
            overwrite_output: true,
            error: None,
        }
    }

    #[test]
    fn background_notices_use_first_output_line_only_when_requested() {
        let mut outcome = result();
        assert_eq!(completion_notice(&outcome), "First line");
        outcome.overwrite_output = false;
        assert_eq!(completion_notice(&outcome), "Example completed");
        outcome.success = false;
        outcome.exit_code = Some(7);
        outcome.stderr = "\nSomething failed\nMore detail".to_owned();
        assert_eq!(
            completion_notice(&outcome),
            "Example failed: Something failed"
        );
        outcome.cancelled = true;
        assert_eq!(completion_notice(&outcome), "Example cancelled");
    }

    #[test]
    fn background_notice_is_bounded_and_strips_control_characters() {
        let mut outcome = result();
        outcome.stdout = format!("\x07{}", "x".repeat(900));
        assert_eq!(completion_notice(&outcome), "x".repeat(512));
    }
}
