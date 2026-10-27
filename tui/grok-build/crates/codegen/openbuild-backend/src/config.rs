//! Session config-option mapping.
//!
//! The kernel advertises `configOptions` on `session/new`, `session/load`,
//! `session/resume`, `session/fork` and pushes `config_option_update` when a
//! selection changes. Those options are the *only* way to change model, effort
//! and mode over ACP, but the TUI drives model/mode changes through the
//! `session/set_model` and `session/set_mode` requests it inherited from
//! grok-shell.
//!
//! This module is the translation table:
//!
//! - `configOptions` → [`ModelCatalog`] (what the `/model` picker shows)
//! - a model selection → `(configId="model", value="<provider>/<id>")`
//! - an effort selection → `(configId="effort", value="<variant>")`
//! - a mode selection → `(configId="mode", value="<agent id>")`
//!
//! Option ids come from the kernel (`config-option.ts`): `model`, `effort`,
//! `mode`. Anything else is ignored rather than guessed at.

use agent_client_protocol as acp;
use serde_json::Value;

/// Kernel-side config option ids.
pub const OPTION_MODEL: &str = "model";
pub const OPTION_EFFORT: &str = "effort";
pub const OPTION_MODE: &str = "mode";

/// A select option as the kernel describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOption {
    pub id: String,
    pub name: String,
    pub current: String,
    pub values: Vec<SelectValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectValue {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

impl SelectOption {
    /// The selected value's display name, falling back to the raw id.
    pub fn current_name(&self) -> &str {
        self.values
            .iter()
            .find(|value| value.value == self.current)
            .map(|value| value.name.as_str())
            .unwrap_or(self.current.as_str())
    }
}

/// All select options advertised for one session, keyed by option id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigOptions {
    pub model: Option<SelectOption>,
    pub effort: Option<SelectOption>,
    pub mode: Option<SelectOption>,
}

impl ConfigOptions {
    /// Parse the wire shape `[{ id, name, category, type: "select", currentValue, options: [...] }]`.
    ///
    /// Unknown option ids and non-select options are skipped: the kernel may add
    /// options this build has never heard of, and guessing at their semantics
    /// would send invalid `set_config_option` calls.
    pub fn parse(options: &[acp::SessionConfigOption]) -> Self {
        let mut parsed = Self::default();
        for option in options {
            let acp::SessionConfigKind::Select(select) = &option.kind else {
                continue;
            };
            let values = flatten_options(&select.options)
                .into_iter()
                .map(|value| SelectValue {
                    value: value.value.to_string(),
                    name: value.name.clone(),
                    description: value.description.clone(),
                })
                .collect();
            let select_option = SelectOption {
                id: option.id.to_string(),
                name: option.name.clone(),
                current: select.current_value.to_string(),
                values,
            };
            match option.id.0.as_ref() {
                OPTION_MODEL => parsed.model = Some(select_option),
                OPTION_EFFORT => parsed.effort = Some(select_option),
                OPTION_MODE => parsed.mode = Some(select_option),
                _ => {}
            }
        }
        parsed
    }

    /// Parse a raw JSON `configOptions` array (e.g. from `InitializeResponse.meta`).
    ///
    /// Returns `None` when the value does not deserialize, so callers can fall
    /// back to their previous state instead of clearing the picker.
    pub fn from_json(value: &Value) -> Option<Self> {
        let options: Vec<acp::SessionConfigOption> = serde_json::from_value(value.clone()).ok()?;
        Some(Self::parse(&options))
    }

    pub fn is_empty(&self) -> bool {
        self.model.is_none() && self.effort.is_none() && self.mode.is_none()
    }

    /// Whether the option is a model selector the kernel understands.
    pub fn model_values(&self) -> &[SelectValue] {
        self.model
            .as_ref()
            .map(|option| option.values.as_slice())
            .unwrap_or_default()
    }

    /// Current model id (`<provider>/<model>`), if any.
    pub fn current_model(&self) -> Option<&str> {
        self.model.as_ref().map(|option| option.current.as_str())
    }

    pub fn current_mode(&self) -> Option<&str> {
        self.mode.as_ref().map(|option| option.current.as_str())
    }
}

/// A `set_config_option` change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChange {
    pub config_id: &'static str,
    pub value: String,
}

impl ConfigChange {
    pub fn model(value: impl Into<String>) -> Self {
        Self {
            config_id: OPTION_MODEL,
            value: value.into(),
        }
    }

    pub fn effort(value: impl Into<String>) -> Self {
        Self {
            config_id: OPTION_EFFORT,
            value: value.into(),
        }
    }

    pub fn mode(value: impl Into<String>) -> Self {
        Self {
            config_id: OPTION_MODE,
            value: value.into(),
        }
    }

    /// Build the ACP request for this change.
    pub fn request(&self, session_id: &acp::SessionId) -> acp::SetSessionConfigOptionRequest {
        acp::SetSessionConfigOptionRequest::new(
            session_id.clone(),
            acp::SessionConfigId::new(self.config_id),
            acp::SessionConfigValueId::new(self.value.clone()),
        )
    }
}

/// Flatten grouped and ungrouped select options into one list.
///
/// The kernel emits ungrouped lists today; grouping exists in the schema for
/// future use, and dropping those values would silently empty the picker.
fn flatten_options(options: &acp::SessionConfigSelectOptions) -> Vec<&acp::SessionConfigSelectOption> {
    match options {
        acp::SessionConfigSelectOptions::Ungrouped(values) => values.iter().collect(),
        acp::SessionConfigSelectOptions::Grouped(groups) => {
            groups.iter().flat_map(|group| group.options.iter()).collect()
        }
        _ => Vec::new(),
    }
}

/// Split a kernel model id (`provider/model`) into grok's flat `ModelId`.
///
/// The TUI shows one flat list, so the provider prefix is preserved verbatim:
/// two providers can ship the same model name and the id has to stay unique.
pub fn model_id_from_kernel(id: &str) -> acp::ModelId {
    acp::ModelId::new(id)
}

/// Rebuild the `configOptions` a `/model` pick should send.
///
/// `selection` is the value the user picked from the catalog. When the kernel
/// advertises an effort option, switching models resets effort to `default`
/// only if the current effort is not offered by the new model — the kernel
/// validates that itself and returns `InvalidEffortError`, so we let it decide
/// and only send the model change here.
pub fn model_change(selection: &str) -> ConfigChange {
    ConfigChange::model(selection)
}

/// True when the backend expects `session/set_config_option` instead of the
/// grok-shell private `session/set_model` / `session/set_mode` requests.
pub fn uses_config_options(options: &ConfigOptions) -> bool {
    !options.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn options_json() -> Value {
        json!([
            {
                "id": "model",
                "name": "Model",
                "category": "model",
                "type": "select",
                "currentValue": "anthropic/claude-sonnet-4",
                "options": [
                    { "value": "anthropic/claude-sonnet-4", "name": "anthropic/Claude Sonnet 4" },
                    { "value": "openai/gpt-5", "name": "openai/GPT-5" }
                ]
            },
            {
                "id": "effort",
                "name": "Effort",
                "category": "thought_level",
                "type": "select",
                "currentValue": "default",
                "options": [
                    { "value": "default", "name": "Default" },
                    { "value": "high", "name": "High" }
                ]
            },
            {
                "id": "mode",
                "name": "Session Mode",
                "category": "mode",
                "type": "select",
                "currentValue": "build",
                "options": [
                    { "value": "build", "name": "Build" },
                    { "value": "plan", "name": "Plan", "description": "Read-only planning" }
                ]
            }
        ])
    }

    #[test]
    fn parses_the_three_kernel_options() {
        let parsed = ConfigOptions::from_json(&options_json()).expect("parse");
        assert_eq!(parsed.current_model(), Some("anthropic/claude-sonnet-4"));
        assert_eq!(parsed.current_mode(), Some("build"));
        assert_eq!(parsed.model_values().len(), 2);
        assert_eq!(
            parsed.model.as_ref().map(SelectOption::current_name),
            Some("anthropic/Claude Sonnet 4")
        );
        assert_eq!(
            parsed
                .mode
                .as_ref()
                .and_then(|mode| mode.values.get(1))
                .and_then(|value| value.description.as_deref()),
            Some("Read-only planning")
        );
    }

    #[test]
    fn ignores_unknown_option_ids() {
        let value = json!([
            { "id": "temperature", "name": "Temperature", "type": "select",
              "currentValue": "1", "options": [{ "value": "1", "name": "1" }] }
        ]);
        let parsed = ConfigOptions::from_json(&value).expect("parse");
        assert!(parsed.is_empty());
        assert!(!uses_config_options(&parsed));
    }

    #[test]
    fn malformed_payload_returns_none() {
        assert!(ConfigOptions::from_json(&json!({ "not": "an array" })).is_none());
    }

    #[test]
    fn builds_config_option_requests() {
        let session = acp::SessionId::new("ses_1");
        let request = ConfigChange::model("openai/gpt-5").request(&session);
        assert_eq!(request.config_id.to_string(), OPTION_MODEL);
        // `unstable_boolean_config` is on, so the value is a tagged enum; the
        // select payload is the `ValueId` arm.
        assert_eq!(
            request
                .value
                .as_value_id()
                .map(|value| value.to_string())
                .as_deref(),
            Some("openai/gpt-5")
        );
        assert_eq!(request.session_id.to_string(), "ses_1");

        let request = ConfigChange::mode("plan").request(&session);
        assert_eq!(request.config_id.to_string(), OPTION_MODE);
        assert_eq!(
            request
                .value
                .as_value_id()
                .map(|value| value.to_string())
                .as_deref(),
            Some("plan")
        );
    }

    #[test]
    fn model_id_keeps_provider_prefix() {
        assert_eq!(
            model_id_from_kernel("openai/gpt-5").to_string(),
            "openai/gpt-5"
        );
    }
}
