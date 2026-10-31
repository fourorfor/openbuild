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

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        if !args.trim().is_empty() {
            return CommandResult::Error("/backend takes no arguments.".to_string());
        }
        match ctx.pane_connection.and_then(report_for_slot) {
            Some(report) => CommandResult::Message(report),
            None => CommandResult::Message(report()),
        }
    }
}

/// Report one connection slot's backend, for `/backend` on a pane.
///
/// The primary slot keeps the process-wide report (it also carries the proxy and
/// feed lines, which are process-wide facts); a secondary slot answers for its
/// own kernel so switching panes changes what `/backend` says.
pub fn report_for_slot(slot: crate::app::connections::ConnectionId) -> Option<String> {
    if slot == crate::app::connections::ConnectionId::PRIMARY {
        return Some(report());
    }
    let profile = openbuild_backend::profile::active_in(slot.0)?;
    let mut lines = Vec::new();
    lines.push(format!("Backend: {}", describe_kind(&profile)));
    if let Some(path) = profile.kernel_path.as_ref() {
        lines.push(format!("Kernel: {}", path.display()));
    }
    lines.push(format!("Capabilities: {}", describe_capabilities(&profile)));
    if let Some(line) = describe_questions(&profile) {
        lines.push(line);
    }
    lines.push(format!(
        "Pane: slot {} · profile {}",
        slot.0,
        openbuild_backend::spawn::profile_root("")
            .map(|_| "independent storage")
            .unwrap_or("shared storage")
    ));
    lines.push(format!(
        "Proxy: {}",
        openbuild_backend::proxy::describe(openbuild_backend::spawn::active_proxy())
    ));
    // Each pane has its own feed; reading the process-wide singleton made a
    // second pane report the primary kernel's counters.
    lines.extend(feed_lines(openbuild_backend::http_feed::active_status_in(
        slot.0,
    )));
    if let Some(progress) = crate::app::tool_progress::last_in(slot.0) {
        lines.push(format!("Tool progress: {progress}"));
    }
    Some(lines.join("\n"))
}

/// The `HTTP feed:` lines for one status, shared by both report paths.
fn feed_lines(status: Option<openbuild_backend::http_feed::FeedStatus>) -> Vec<String> {
    let Some(status) = status else {
        return vec!["HTTP feed: unavailable for this pane".to_string()];
    };
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
    let mut lines = vec![line];
    if let Some(error) = status.last_error.as_deref() {
        lines.push(format!("HTTP feed error: {error}"));
    }
    lines
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
    lines.push(format!("Backend: {}", describe_kind(&profile)));
    if let Some(path) = profile.kernel_path.as_ref() {
        lines.push(format!("Kernel: {}", path.display()));
    }
    lines.push(format!("Capabilities: {}", describe_capabilities(&profile)));
    if let Some(line) = describe_questions(&profile) {
        lines.push(line);
    }
    lines.push(format!(
        "Proxy: {}",
        openbuild_backend::proxy::describe(openbuild_backend::spawn::active_proxy())
    ));
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
    if let Some(progress) = crate::app::tool_progress::last_in(0) {
        lines.push(format!("Tool progress: {progress}"));
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
    if caps.question_ext {
        on.push("question extension");
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

/// State the question channel explicitly, including the degraded case.
///
/// `/backend` used to imply that form elicitation worked, which hid the real
/// cause of the "user dismissed this question" failure. Say which channel the
/// questions actually travel over.
fn describe_questions(profile: &openbuild_backend::BackendProfile) -> Option<String> {
    let caps = &profile.caps;
    if caps.question_ext {
        return Some(
            "Questions: ask_user_question extension (forms render in this client)".to_string(),
        );
    }
    if caps.form_elicitation {
        return Some("Questions: ACP form elicitation".to_string());
    }
    if profile.is_opencode() {
        return Some(
            "Questions: unsupported (question falls back to assumptions)".to_string(),
        );
    }
    None
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

    fn profile_with(
        mutate: impl FnOnce(&mut openbuild_backend::Capabilities),
    ) -> openbuild_backend::BackendProfile {
        let mut profile = openbuild_backend::BackendProfile::opencode(None);
        mutate(&mut profile.caps);
        profile
    }

    /// `/backend` must not imply that form elicitation works when the client
    /// cannot decode it; it names the channel questions actually use.
    #[test]
    fn names_the_question_extension_instead_of_form_elicitation() {
        let profile = profile_with(|caps| {
            caps.form_elicitation = false;
            caps.question_ext = true;
        });
        let line = describe_questions(&profile).expect("a question line");
        assert!(line.contains("ask_user_question extension"), "{line}");
        assert!(!describe_capabilities(&profile).contains("form elicitation"));
    }

    /// With neither channel, the report says so and does not claim a feature.
    #[test]
    fn reports_questions_as_unsupported_without_a_channel() {
        let profile = profile_with(|caps| {
            caps.form_elicitation = false;
            caps.question_ext = false;
        });
        let line = describe_questions(&profile).expect("a question line");
        assert!(line.contains("unsupported"), "{line}");
        assert!(line.contains("assumptions"), "{line}");
    }
}
