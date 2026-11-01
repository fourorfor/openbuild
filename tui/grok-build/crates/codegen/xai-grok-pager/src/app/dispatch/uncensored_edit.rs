//! `/uncensored-edit`: edit the policy, switch its source, and tune the switches.
//!
//! The policy text is a file the plugin reads on every turn, so saving here takes
//! effect without a kernel restart. Every save snapshots the previous text into
//! `uncensored/history/` and bumps `policyRev`, which the injected marker carries.

use crate::app::actions::Effect;
use crate::app::app_view::{ActiveView, AppView};
use openbuild_backend::plugin::{
    self, PolicySource, ProviderRewrite, TextRefusalProbe, UncensoredConfig,
};
use std::path::PathBuf;

/// One parsed `/uncensored-edit` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EditAction {
    /// Open `policy.md` (the default) in the external editor.
    Edit,
    EditVariant(String),
    List,
    Diff,
    Reset,
    SetModel(String),
    SetSource(PolicySource),
    KeepMarker(bool),
    SetProviderRewrite(ProviderRewrite),
    DisableProbe,
    /// Per-session budget for automatic refusal retries (0-10).
    SetRetryBudget(u64),
    /// Rewrite the model's own last refusal out of the request history.
    SetStripHistory(bool),
    ShowHelp,
}

pub(crate) fn parse_args(args: &str) -> Result<EditAction, String> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    let Some(first) = parts.first() else {
        return Ok(EditAction::Edit);
    };
    let rest = parts.get(1).copied();
    let no_more = |rest: Option<&str>| match rest {
        None => Ok(()),
        Some(extra) => Err(format!("unexpected argument: {extra}")),
    };
    match *first {
        "--list" | "-l" => no_more(rest).map(|()| EditAction::List),
        "--diff" => no_more(rest).map(|()| EditAction::Diff),
        "--reset" => no_more(rest).map(|()| EditAction::Reset),
        "--use-full" | "--use-hanshuang" => {
            no_more(rest).map(|()| EditAction::SetSource(PolicySource::Hanshuang))
        }
        "--use-compact" => no_more(rest).map(|()| EditAction::SetSource(PolicySource::Compact)),
        "--use-hybrid" => no_more(rest).map(|()| EditAction::SetSource(PolicySource::Hybrid)),
        "--keep-marker" => no_more(rest).map(|()| EditAction::KeepMarker(true)),
        "--strip-marker" => no_more(rest).map(|()| EditAction::KeepMarker(false)),
        "--disable-probe" => no_more(rest).map(|()| EditAction::DisableProbe),
        "--retry-budget" => match rest.and_then(|value| value.parse::<u64>().ok()) {
            Some(budget) if budget <= 10 => Ok(EditAction::SetRetryBudget(budget)),
            _ => Err("--retry-budget needs an integer between 0 and 10".to_string()),
        },
        "--strip-history" => match rest {
            Some("on") => Ok(EditAction::SetStripHistory(true)),
            Some("off") => Ok(EditAction::SetStripHistory(false)),
            _ => Err("--strip-history needs on or off".to_string()),
        },
        "--variant" => match rest {
            Some(id) if valid_id(id) => Ok(EditAction::EditVariant(id.to_string())),
            Some(id) => Err(format!("variant must be a plain id, got {id:?}")),
            None => Err("--variant needs an id, e.g. --variant anthropic".to_string()),
        },
        "--model" => match rest {
            Some(reference) if reference.contains('/') => {
                Ok(EditAction::SetModel(reference.to_string()))
            }
            Some(reference) => Err(format!(
                "--model needs \"provider/model\", got {reference:?}"
            )),
            None => Err("--model needs \"provider/model\"".to_string()),
        },
        "--provider-rewrite" => match rest {
            Some("on") | Some("auto") => Ok(EditAction::SetProviderRewrite(ProviderRewrite::Auto)),
            Some("off") => Ok(EditAction::SetProviderRewrite(ProviderRewrite::Off)),
            Some(other) => Err(format!(
                "--provider-rewrite must be on, auto, or off, got {other:?}"
            )),
            None => Err("--provider-rewrite needs on, auto, or off".to_string()),
        },
        other => Err(format!(
            "unknown /uncensored-edit argument: {other}. Use --list, --diff, --reset, --variant <id>, \
             --model <ref>, --use-hanshuang, --use-compact, --use-hybrid, --keep-marker, \
             --provider-rewrite on|off, --disable-probe, --retry-budget <0-10>, \
             or --strip-history on|off."
        )),
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
}

/// Dispatch one `/uncensored-edit` invocation.
pub(crate) fn dispatch_uncensored_edit(app: &mut AppView, args: &str) -> Vec<Effect> {
    let action = match parse_args(args) {
        Ok(action) => action,
        Err(message) => {
            app.show_toast(&message);
            return vec![];
        }
    };
    let Some(config_dir) = plugin::effective_config_dir() else {
        app.show_toast("No config directory; /uncensored-edit is unavailable");
        return vec![];
    };
    let config = plugin::uncensored_config();
    if let Some(error) = config.error.as_deref() {
        app.show_toast(&format!("Fix uncensored.json first: {error}"));
        return vec![];
    }
    match action {
        EditAction::Edit => {
            open_policy(app, &config_dir, &config);
            vec![]
        }
        EditAction::EditVariant(id) => {
            open_variant(app, &config_dir, &id);
            vec![]
        }
        EditAction::List => {
            push_block(app, &list_variants(&config_dir, &config));
            vec![]
        }
        EditAction::Diff => {
            push_block(app, &diff_against_default(&config_dir));
            vec![]
        }
        EditAction::Reset => {
            match reset_policy(&config_dir) {
                Ok(()) => {
                    push_block(
                        app,
                        "Policy reset from policy.default.md; the previous text is in uncensored/history/.",
                    );
                }
                Err(error) => app.show_toast(&format!("Reset failed: {error}")),
            }
            vec![]
        }
        EditAction::SetModel(reference) => {
            match plugin::update_uncensored_config(|document| {
                document.insert("model".into(), serde_json::json!(reference));
            }) {
                Ok(_) => push_block(
                    app,
                    &format!("Uncensored model set to {reference}; applies to new turns."),
                ),
                Err(error) => app.show_toast(&format!("Could not save the model: {error}")),
            }
            vec![]
        }
        EditAction::SetSource(source) => {
            match plugin::update_uncensored_config(|document| {
                document.insert("policySource".into(), serde_json::json!(source.as_str()));
            }) {
                Ok(_) => {
                    let mut message =
                        format!("Policy source: {}.", source.label());
                    if source == PolicySource::Hybrid {
                        message.push_str(
                            " hybrid injects the compact kernel and then the hanshuang body; \
                             a model that follows the last instruction best can ignore the kernel.",
                        );
                    }
                    push_block(app, &message);
                }
                Err(error) => app.show_toast(&format!("Could not save the policy source: {error}")),
            }
            vec![]
        }
        EditAction::KeepMarker(keep) => {
            match plugin::update_uncensored_config(|document| {
                document.insert(
                    "stripChatTemplateMarker".into(),
                    serde_json::json!(!keep),
                );
            }) {
                Ok(_) => push_block(
                    app,
                    if keep {
                        "Marker: kept (the first ChatML line is injected verbatim)."
                    } else {
                        "Marker: stripped (the first ChatML line is removed at injection; the file is unchanged)."
                    },
                ),
                Err(error) => app.show_toast(&format!("Could not save the marker setting: {error}")),
            }
            vec![]
        }
        EditAction::SetProviderRewrite(mode) => {
            match plugin::update_uncensored_config(|document| {
                document.insert("providerRewrite".into(), serde_json::json!(mode.as_str()));
            }) {
                Ok(_) => push_block(
                    app,
                    &format!(
                        "Provider rewrite: {}.{}",
                        mode.as_str(),
                        if mode == ProviderRewrite::Off {
                            " No http hook is installed."
                        } else {
                            " Installed for providers with a rule or a user body overlay."
                        }
                    ),
                ),
                Err(error) => app.show_toast(&format!("Could not save providerRewrite: {error}")),
            }
            vec![]
        }
        EditAction::DisableProbe => {
            match plugin::update_uncensored_config(|document| {
                document.insert(
                    "textRefusalProbe".into(),
                    serde_json::json!(TextRefusalProbe::Off.as_str()),
                );
            }) {
                Ok(_) => push_block(app, "Text refusal probe: off."),
                Err(error) => app.show_toast(&format!("Could not disable the probe: {error}")),
            }
            vec![]
        }
        EditAction::SetRetryBudget(budget) => {
            match plugin::update_uncensored_config(|document| {
                document.insert("refusalRetryBudget".into(), serde_json::json!(budget));
            }) {
                Ok(_) => push_block(
                    app,
                    &format!("Refusal retry budget: {budget} (0 disables automatic retries)."),
                ),
                Err(error) => app.show_toast(&format!("Could not save the retry budget: {error}")),
            }
            vec![]
        }
        EditAction::SetStripHistory(on) => {
            match plugin::update_uncensored_config(|document| {
                document.insert("stripRefusalHistory".into(), serde_json::json!(on));
            }) {
                Ok(_) => push_block(
                    app,
                    &format!(
                        "Refusal history rewrite: {}.",
                        if on {
                            "on (the model's own last refusal is removed before each request)"
                        } else {
                            "off"
                        }
                    ),
                ),
                Err(error) => app.show_toast(&format!("Could not save the setting: {error}")),
            }
            vec![]
        }
        EditAction::ShowHelp => vec![],
    }
}

fn open_policy(app: &mut AppView, config_dir: &std::path::Path, config: &UncensoredConfig) {
    let path = plugin::uncensored_policy_path(config_dir);
    // A first edit starts from the built-in default so the file is never empty.
    if !path.is_file() {
        let default = std::fs::read_to_string(plugin::uncensored_policy_default_path(config_dir))
            .unwrap_or_else(|_| plugin::POLICY_DEFAULT.to_string());
        if let Err(error) = std::fs::create_dir_all(
            path.parent().unwrap_or(config_dir),
        )
        .and_then(|()| std::fs::write(&path, default))
        {
            app.show_toast(&format!("Could not create policy.md: {error}"));
            return;
        }
    }
    match snapshot_policy(config_dir, &path) {
        Ok(()) => {}
        Err(error) => {
            app.show_toast(&format!("Could not snapshot the policy: {error}"));
            return;
        }
    }
    // The revision is bumped before the editor opens so the marker on the next
    // turn differs even if the user saves the same text back.
    let rev = config.policy_rev.saturating_add(1);
    let _ = plugin::update_uncensored_config(|document| {
        document.insert("policyRev".into(), serde_json::json!(rev));
        document.insert(
            "policySha256".into(),
            serde_json::json!(file_sha256(&path)),
        );
    });
    request_editor(app, path);
}

fn open_variant(app: &mut AppView, config_dir: &std::path::Path, id: &str) {
    let dir = plugin::uncensored_variants_dir(config_dir);
    let path = dir.join(format!("{id}.md"));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        app.show_toast(&format!("Could not create the variants directory: {error}"));
        return;
    }
    if !path.is_file()
        && let Err(error) = std::fs::write(
            &path,
            format!("# Variant: {id}\n- Only the differences from the base policy.\n"),
        )
    {
        app.show_toast(&format!("Could not create the variant: {error}"));
        return;
    }
    request_editor(app, path);
}

/// Hand the file to the external editor through the existing pipeline.
fn request_editor(app: &mut AppView, path: PathBuf) {
    app.pending_editor = Some(
        crate::app::external_editor::PendingEditorRequest::ConfigFile {
            path,
            refresh_agents_modal: None,
        },
    );
    app.show_toast("Opening the policy in your editor…");
}

fn list_variants(config_dir: &std::path::Path, config: &UncensoredConfig) -> String {
    let mut lines = vec![format!("Policy source: {}", config.policy_source.as_str())];
    lines.push(format!("Variant: {}", config.variant));
    lines.push(format!(
        "Policy: {}",
        plugin::uncensored_policy_path(config_dir).display()
    ));
    let dir = plugin::uncensored_variants_dir(config_dir);
    let mut found: Vec<String> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
                .filter(|name| name.ends_with(".md"))
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    if found.is_empty() {
        lines.push("Variants: none yet (/uncensored-edit --variant <id> creates one)".to_string());
    } else {
        lines.push(format!("Variants: {}", found.join(", ")));
    }
    if let Ok(text) = std::fs::read_to_string(plugin::uncensored_policy_hanshuang_path(config_dir)) {
        lines.push(format!(
            "Hanshuang body: {} bytes, sha256 {} (policy.hanshuang.md)",
            text.len(),
            file_sha256(&plugin::uncensored_policy_hanshuang_path(config_dir))
                .chars()
                .take(12)
                .collect::<String>()
        ));
    }
    lines.join("\n")
}

fn diff_against_default(config_dir: &std::path::Path) -> String {
    let policy = std::fs::read_to_string(plugin::uncensored_policy_path(config_dir));
    let default = std::fs::read_to_string(plugin::uncensored_policy_default_path(config_dir))
        .unwrap_or_else(|_| plugin::POLICY_DEFAULT.to_string());
    match policy {
        Ok(policy) if policy == default => "policy.md matches policy.default.md".to_string(),
        Ok(policy) => {
            let mut lines = vec!["policy.md differs from policy.default.md:".to_string()];
            for (index, line) in policy.lines().enumerate() {
                if default.lines().nth(index) != Some(line) {
                    lines.push(format!("+{}: {line}", index + 1));
                }
            }
            lines.push("(/uncensored-edit --reset restores the default)".to_string());
            lines.join("\n")
        }
        Err(_) => "policy.md does not exist yet; /uncensored-edit creates it from the default"
            .to_string(),
    }
}

fn reset_policy(config_dir: &std::path::Path) -> Result<(), String> {
    let path = plugin::uncensored_policy_path(config_dir);
    if path.is_file() {
        snapshot_policy(config_dir, &path)?;
    }
    let default = std::fs::read_to_string(plugin::uncensored_policy_default_path(config_dir))
        .unwrap_or_else(|_| plugin::POLICY_DEFAULT.to_string());
    std::fs::create_dir_all(path.parent().unwrap_or(config_dir))
        .map_err(|error| error.to_string())?;
    std::fs::write(&path, default).map_err(|error| error.to_string())?;
    Ok(())
}

/// Copy the current policy into `uncensored/history/` with a sortable stamp.
fn snapshot_policy(config_dir: &std::path::Path, path: &std::path::Path) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let dir = plugin::uncensored_history_dir(config_dir);
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    std::fs::write(dir.join(format!("policy.{stamp}.md")), text)
        .map_err(|error| error.to_string())
}

fn file_sha256(path: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    match std::fs::read(path) {
        Ok(bytes) => hasher.update(bytes),
        Err(_) => return String::new(),
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn push_block(app: &mut AppView, message: &str) {
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get_mut(&id)
    {
        agent
            .scrollback
            .push_block(crate::scrollback::block::RenderBlock::system(message));
        return;
    }
    app.show_toast(message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_command_edits_the_policy() {
        assert_eq!(parse_args(""), Ok(EditAction::Edit));
        assert_eq!(parse_args("   "), Ok(EditAction::Edit));
    }

    #[test]
    fn list_and_diff_are_recognized() {
        assert_eq!(parse_args("--list"), Ok(EditAction::List));
        assert_eq!(parse_args("--diff"), Ok(EditAction::Diff));
        assert_eq!(parse_args("--reset"), Ok(EditAction::Reset));
    }

    #[test]
    fn source_switches_map_to_the_policy_source() {
        assert_eq!(
            parse_args("--use-hanshuang"),
            Ok(EditAction::SetSource(PolicySource::Hanshuang))
        );
        // The v3 spelling keeps working and lands on the same slot.
        assert_eq!(
            parse_args("--use-full"),
            Ok(EditAction::SetSource(PolicySource::Hanshuang))
        );
        assert_eq!(
            parse_args("--use-compact"),
            Ok(EditAction::SetSource(PolicySource::Compact))
        );
        assert_eq!(
            parse_args("--use-hybrid"),
            Ok(EditAction::SetSource(PolicySource::Hybrid))
        );
    }

    #[test]
    fn marker_switch_inverts_into_strip_chat_template_marker() {
        assert_eq!(parse_args("--keep-marker"), Ok(EditAction::KeepMarker(true)));
        assert_eq!(
            parse_args("--strip-marker"),
            Ok(EditAction::KeepMarker(false))
        );
    }

    #[test]
    fn variant_and_model_validate_their_values() {
        assert_eq!(
            parse_args("--variant anthropic"),
            Ok(EditAction::EditVariant("anthropic".to_string()))
        );
        assert!(parse_args("--variant ../etc").is_err());
        assert!(parse_args("--variant").is_err());
        assert_eq!(
            parse_args("--model local/qwen3-32b"),
            Ok(EditAction::SetModel("local/qwen3-32b".to_string()))
        );
        assert!(parse_args("--model qwen3").is_err());
    }

    #[test]
    fn provider_rewrite_accepts_on_auto_and_off() {
        assert_eq!(
            parse_args("--provider-rewrite off"),
            Ok(EditAction::SetProviderRewrite(ProviderRewrite::Off))
        );
        assert_eq!(
            parse_args("--provider-rewrite on"),
            Ok(EditAction::SetProviderRewrite(ProviderRewrite::Auto))
        );
        assert!(parse_args("--provider-rewrite maybe").is_err());
    }

    #[test]
    fn unknown_flags_name_the_valid_options() {
        let error = parse_args("--nope").expect_err("must fail");
        assert!(error.contains("--use-hanshuang"), "{error}");
        assert!(error.contains("--retry-budget"), "{error}");
        assert!(error.contains("--strip-history"), "{error}");
    }

    #[test]
    fn retry_budget_and_history_rewrite_parse_their_values() {
        assert_eq!(parse_args("--retry-budget 0"), Ok(EditAction::SetRetryBudget(0)));
        assert_eq!(parse_args("--retry-budget 3"), Ok(EditAction::SetRetryBudget(3)));
        assert!(parse_args("--retry-budget 11").is_err());
        assert!(parse_args("--retry-budget abc").is_err());
        assert!(parse_args("--retry-budget").is_err());
        assert_eq!(parse_args("--strip-history on"), Ok(EditAction::SetStripHistory(true)));
        assert_eq!(parse_args("--strip-history off"), Ok(EditAction::SetStripHistory(false)));
        assert!(parse_args("--strip-history maybe").is_err());
    }
}
