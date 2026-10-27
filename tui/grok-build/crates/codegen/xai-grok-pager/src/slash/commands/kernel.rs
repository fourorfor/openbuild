//! `/kernel` (aliases `/plugins-kernel`, `/mcp`): manage the OpenBuild kernel's
//! own plugins and MCP servers.
//!
//! The kernel already ships first-class `plugin` and `mcp` subcommands, so the
//! TUI does not reimplement any of it — it shells out to the kernel binary that
//! is already on disk and prints the result. That keeps one source of truth for
//! the config schema and means a kernel upgrade automatically upgrades this
//! panel.
//!
//! Subcommands:
//!
//! - `/kernel plugin list` — installed plugins (global + project)
//! - `/kernel plugin check` — loaded state and update status for each plugin
//! - `/kernel plugin add <spec>` — install a plugin package
//! - `/kernel plugin remove <name>` — uninstall
//! - `/kernel mcp list` — configured MCP servers
//! - `/kernel mcp add <name> -- <command…>` — add a local server
//! - `/kernel mcp add <name> --url <url>` — add a remote server
//! - `/kernel mcp remove <name>` — delete a server
//! - `/kernel restart` — restart the background service so removals take effect
//! - `/kernel` — print the summary (same as `plugin list` + `mcp list`)
//!
//! The "removals need a restart" caveat is not cosmetic: the kernel's
//! long-running service hot-loads additions but never drops a provider or MCP
//! server that disappeared from config. See `docs/openbuild-providers.md`.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::slash::command::{CommandResult, CommandExecCtx, SlashCommand, slash_meta};

/// Wall-clock ceiling for a kernel subcommand. `plugin add` may hit the network;
/// everything else is local, and a hung child must not freeze the TUI.
const TIMEOUT: Duration = Duration::from_secs(120);

pub struct KernelCommand;

impl SlashCommand for KernelCommand {
    slash_meta! {
        name: "kernel",
        aliases: ["plugins-kernel"],
        description: "Manage kernel plugins and MCP servers",
        usage: "/kernel <plugin|mcp|restart> [...]",
        takes_args: true,
        args_required: false,
        arg_placeholder: "<plugin|mcp|restart>",
    }

    /// Only meaningful on the ACP-subprocess backend: it is the only one with a
    /// kernel binary whose plugin/MCP catalogs these subcommands read.
    fn visible(&self, _ctx: &crate::slash::command::AppCtx) -> bool {
        openbuild_backend::profile::is_opencode()
    }

    fn suggest_args(
        &self,
        _ctx: &crate::slash::command::AppCtx,
        _args_query: &str,
    ) -> Option<Vec<crate::slash::command::ArgItem>> {
        let item = |display: &str, insert: &str, description: &str| {
            crate::slash::command::ArgItem {
                display: display.to_string(),
                match_text: format!("{display} {description}"),
                insert_text: insert.to_string(),
                description: description.to_string(),
            }
        };
        Some(vec![
            item("plugin list", "plugin list", "List installed kernel plugins"),
            item(
                "plugin check",
                "plugin check",
                "Check loaded state and updates for kernel plugins",
            ),
            item("plugin add", "plugin add ", "Install a kernel plugin package"),
            item("plugin remove", "plugin remove ", "Uninstall a kernel plugin"),
            item("mcp list", "mcp list", "List configured MCP servers"),
            item("mcp add", "mcp add ", "Add an MCP server"),
            item("mcp remove", "mcp remove ", "Remove an MCP server"),
            item(
                "restart",
                "restart",
                "Restart the kernel service (picks up removals)",
            ),
        ])
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        let Some(kernel) = kernel_binary() else {
            return CommandResult::Error(
                "Kernel binary not found. Set OPENBUILD_KERNEL or place \
                 openbuild-kernel next to the TUI."
                    .to_string(),
            );
        };

        // Bare `/kernel` reports both catalogs; that is the "what do I have?"
        // question, and it is the one users ask most.
        if trimmed.is_empty() {
            let mut out = Vec::new();
            out.push("Plugins:".to_string());
            out.push(indent(&run_kernel(&kernel, &["plugin", "list"])));
            out.push(String::new());
            out.push("MCP servers:".to_string());
            out.push(indent(&run_kernel(&kernel, &["mcp", "list"])));
            out.push(String::new());
            out.push(
                "Changes to plugins or MCP servers need `/kernel restart` before removals \
                 take effect."
                    .to_string(),
            );
            return CommandResult::Message(out.join("\n"));
        }

        let mut parts = trimmed.split_whitespace();
        let group = parts.next().unwrap_or_default();
        let rest: Vec<&str> = parts.collect();

        match group {
            "restart" => restart_service(&kernel),
            "plugin" => {
                let Some(verb) = rest.first() else {
                    return CommandResult::Error(
                        "Usage: /kernel plugin <list|check|add|remove> [...]".to_string(),
                    );
                };
                match *verb {
                    "list" | "check" | "add" | "remove" => {
                        let mut argv: Vec<&str> = vec!["plugin"];
                        argv.extend(rest.iter().copied());
                        CommandResult::Message(run_kernel(&kernel, &argv))
                    }
                    other => CommandResult::Error(format!(
                        "Unknown plugin verb: {other}. Use list, check, add, or remove."
                    )),
                }
            }
            "mcp" => {
                let Some(verb) = rest.first() else {
                    return CommandResult::Error(
                        "Usage: /kernel mcp <list|add|remove> [...]".to_string(),
                    );
                };
                match *verb {
                    "list" | "add" | "remove" => {
                        let mut argv: Vec<&str> = vec!["mcp"];
                        argv.extend(rest.iter().copied());
                        CommandResult::Message(run_kernel(&kernel, &argv))
                    }
                    other => CommandResult::Error(format!(
                        "Unknown mcp verb: {other}. Use list, add, or remove."
                    )),
                }
            }
            other => CommandResult::Error(format!(
                "Unknown target: {other}. Use plugin, mcp, or restart."
            )),
        }
    }
}

/// Locate the kernel the same way the backend does.
fn kernel_binary() -> Option<PathBuf> {
    openbuild_backend::spawn::resolve_kernel_path(None)
}

/// Run a kernel subcommand and return its combined output.
///
/// `--standalone` is deliberately **not** used: the CLI otherwise starts (or
/// reuses) the long-running service, which is exactly what the user wants when
/// they are about to restart it, and it keeps the command fast.
fn run_kernel(kernel: &std::path::Path, argv: &[&str]) -> String {
    let mut command = Command::new(kernel);
    command.args(argv);
    // The kernel's CLI reads stdin for non-interactive mode; an empty object is
    // the documented "no input" form and keeps it from waiting on the terminal.
    command.stdin(std::process::Stdio::piped());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    command.creation_flags_no_window();

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return format!("failed to start the kernel: {e}"),
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(b"{}");
    }

    match wait_with_timeout(child, TIMEOUT) {
        Ok(text) if text.is_empty() => "no output".to_string(),
        Ok(text) => text,
        Err(msg) => msg,
    }
}

/// `/kernel restart`: stop the background service so config removals apply.
///
/// It also clears the TUI's own reconnect latch, so a user who restarts the
/// service after a crash does not have to type `/reconnect` separately.
fn restart_service(kernel: &std::path::Path) -> CommandResult {
    let output = run_kernel(kernel, &["service", "restart"]);
    let crashed = output.to_lowercase().contains("error")
        || output.to_lowercase().contains("failed")
        || output.to_lowercase().contains("not found");
    if crashed {
        return CommandResult::Error(format!(
            "Kernel service restart failed:\n{output}\n\n\
             If this is a crash rather than a config change, use `/reconnect`."
        ));
    }
    CommandResult::Message(format!(
        "Kernel service restarted. Config removals now apply.\n{output}"
    ))
}

/// Prefix every line so nested output reads as a block in the transcript.
fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Wait for a child with a deadline, killing it on timeout.
///
/// `std::process::Child::wait` has no timeout and the TUI is single-threaded
/// here, so a hung kernel would freeze the whole interface. Both pipes are
/// drained on their own threads: a child that writes more than one pipe buffer
/// would otherwise block forever while the parent is only polling `try_wait`.
fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<String, String> {
    use std::io::Read;

    let reader = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = reader(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let stderr = reader(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );

    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "kernel command timed out after {}s",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(format!("failed to wait for the kernel: {e}")),
        }
    }

    let mut out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    if !err.is_empty() {
        if !out.is_empty() {
            out.push(b'\n');
        }
        out.extend_from_slice(&err);
    }
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

/// Keep console windows from flashing on Windows when spawning the kernel.
trait NoWindow {
    fn creation_flags_no_window(&mut self);
}

impl NoWindow for Command {
    fn creation_flags_no_window(&mut self) {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW
            self.creation_flags(0x0800_0000);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;

    fn ctx<'a>(models: &'a ModelState, bundle: &'a BundleState) -> CommandExecCtx<'a> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        }
    }

    #[test]
    fn indent_prefixes_every_line() {
        assert_eq!(indent("a\nb"), "  a\n  b");
        // `str::lines()` yields nothing for an empty string, so the result stays
        // empty rather than becoming a stray two-space line.
        assert_eq!(indent(""), "");
    }

    #[test]
    fn bare_kernel_without_binary_reports_a_path_hint() {
        // `resolve_kernel_path` cannot find a kernel in the test process unless
        // OPENBUILD_KERNEL happens to point at a real file.
        if kernel_binary().is_some() {
            return;
        }
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut c = ctx(&models, &bundle);
        match KernelCommand.run(&mut c, "") {
            CommandResult::Error(msg) => assert!(msg.contains("OPENBUILD_KERNEL"), "{msg}"),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_target_is_rejected() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut c = ctx(&models, &bundle);
        // Either the kernel is missing (path hint) or the target is rejected;
        // both are user-facing errors, and neither may panic.
        match KernelCommand.run(&mut c, "bogus") {
            CommandResult::Error(msg) => assert!(
                msg.contains("plugin, mcp, or restart") || msg.contains("OPENBUILD_KERNEL"),
                "{msg}"
            ),
            other => panic!("expected an error, got {other:?}"),
        }
    }
}
