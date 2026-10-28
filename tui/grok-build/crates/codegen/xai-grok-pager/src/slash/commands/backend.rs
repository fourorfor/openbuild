//! `/backend`: show which agent backend this process is talking to.
//!
//! The TUI can run against either the original in-process shell or the
//! OpenBuild kernel over ACP. Which one it is decides which features exist
//! (queue, rewind, workflows, …), so the answer has to be inspectable from
//! inside the app rather than inferred from a missing menu row.

use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

/// Report the active backend and the capabilities it advertised.
pub struct BackendCommand;

impl SlashCommand for BackendCommand {
    slash_meta! {
        name: "backend",
        description: "Show the active agent backend and its capabilities",
        usage: "/backend",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        if !args.trim().is_empty() {
            return CommandResult::Error("/backend takes no arguments.".to_string());
        }
        CommandResult::Message(report())
    }
}

/// Build the user-visible report.
///
/// Written as a plain block of text rather than a table: the transcript renders
/// monospace text, and a table would need width-dependent layout.
pub fn report() -> String {
    let Some(profile) = openbuild_backend::profile::active() else {
        return format!(
            "Backend: not connected yet.\n\
             Set OPENBUILD_BACKEND={} to force the kernel, or place \
             {} next to this executable.",
            "opencode",
            openbuild_backend::spawn::kernel_file_name(),
        );
    };

    let mut lines = Vec::new();
    lines.push(format!("Backend: {}", describe_kind(profile)));
    if let Some(path) = profile.kernel_path.as_ref() {
        lines.push(format!("Kernel: {}", path.display()));
    }
    lines.push(format!("Capabilities: {}", describe_capabilities(profile)));
    if let Some(status) = openbuild_backend::http_feed::active_status() {
        let mut line = format!(
            "HTTP feed: {} ({} events, {} dropped, {} reconnects)",
            status.state.label(),
            status.received,
            status.dropped,
            status.reconnects
        );
        if let Some(kind) = status.last_event.as_deref() {
            line.push_str(&format!(", last: {kind}"));
        }
        lines.push(line);
        if let Some(error) = status.last_error.as_deref() {
            lines.push(format!("HTTP feed error: {error}"));
        }
    }
    lines.join("\n")
}

fn describe_kind(profile: &openbuild_backend::BackendProfile) -> String {
    match profile.kind {
        openbuild_backend::BackendKind::OpenCode => {
            format!("{} (ACP subprocess)", profile.name)
        }
        openbuild_backend::BackendKind::GrokShell => {
            format!("{} (in-process shell)", profile.name)
        }
    }
}

/// List the capability groups that are on, so a missing feature can be traced to
/// the backend rather than to the UI.
fn describe_capabilities(profile: &openbuild_backend::BackendProfile) -> String {
    let caps = &profile.caps;
    let mut on = Vec::new();
    if caps.load_session {
        on.push("session/load");
    }
    if caps.session_list {
        on.push("session/list");
    }
    if caps.session_fork {
        on.push("session/fork");
    }
    if caps.session_resume {
        on.push("session/resume");
    }
    if caps.session_delete {
        on.push("session/delete");
    }
    if caps.image_prompt {
        on.push("image prompts");
    }
    if caps.embedded_context {
        on.push("embedded context");
    }
    if caps.additional_directories {
        on.push("extra directories");
    }
    if caps.config_options {
        on.push("config options (model/effort/mode)");
    }
    if caps.form_elicitation {
        on.push("form elicitation");
    }
    if caps.child_session_updates {
        on.push("child session updates");
    }
    if caps.ext_queue {
        on.push("queue extension");
    }
    if caps.ext_rewind {
        on.push("rewind extension");
    }
    if caps.ext_subagents {
        on.push("subagent extension");
    }
    if caps.ext_memory {
        on.push("memory extension");
    }
    if on.is_empty() {
        "none advertised".to_string()
    } else {
        on.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::slash::commands::tests::make_ctx;

    fn ctx() -> CommandExecCtx<'static> {
        // The report only reads the process-wide profile, so the ctx contents
        // do not matter beyond satisfying the signature.
        let models: &'static ModelState = Box::leak(Box::new(ModelState::default()));
        make_ctx(models)
    }

    #[test]
    fn rejects_arguments() {
        let mut c = ctx();
        assert!(matches!(
            BackendCommand.run(&mut c, "now"),
            CommandResult::Error(_)
        ));
    }

    #[test]
    fn reports_something_without_a_connection() {
        // No profile is installed in unit tests, so the not-connected branch runs.
        let mut c = ctx();
        let CommandResult::Message(text) = BackendCommand.run(&mut c, "") else {
            panic!("expected a message");
        };
        assert!(text.contains("Backend:"), "{text}");
    }
}
