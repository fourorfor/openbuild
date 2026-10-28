//! Third-party model providers written by the TUI's `/login` wizard.
//!
//! The kernel owns provider configuration, so this module only edits the
//! JSON file the kernel already reads. It deliberately targets
//! `openbuild.json` — the *second* priority name — instead of
//! `openbuild.jsonc`:
//!
//! - `providers` is merged per provider id (verified against a real kernel:
//!   a provider added in a lower-priority file still appears when a
//!   higher-priority file also defines providers), so writing a separate
//!   file never has to rewrite — and never has to reformat, or lose the
//!   comments in — a hand-written `openbuild.jsonc`;
//! - the file we write stays strict JSON, which is what we can parse back
//!   without a JSONC reader;
//! - the one thing the lower-priority file cannot win is the top-level
//!   `model` default, so [`SaveOutcome::shadowed_model_default`] reports
//!   that case instead of silently doing nothing.
//!
//! Validation happens before any write: an invalid draft leaves the user's
//! file byte-identical.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// Provider implementation packages the wizard can target.
pub const OPENAI_COMPATIBLE_PACKAGE: &str = "@opencode/ai/providers/openai-compatible";
pub const ANTHROPIC_COMPATIBLE_PACKAGE: &str = "@opencode/ai/providers/anthropic-compatible";

/// The file this module owns. Second priority, above the upstream names and
/// below `openbuild.jsonc`.
pub const MANAGED_FILE: &str = "openbuild.json";

/// The only file that can shadow our `model` default.
const SHADOWING_FILE: &str = "openbuild.jsonc";

/// Default context window when the user leaves the field empty.
pub const DEFAULT_CONTEXT: u64 = 128_000;
/// Default output limit when the user leaves the field empty.
pub const DEFAULT_OUTPUT: u64 = 16_384;

/// A provider the user wants to add, already collected by the TUI wizard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDraft {
    /// Provider id: the left half of `provider/model`.
    pub id: String,
    /// Display name shown in `/model` and `/backend`.
    pub name: String,
    /// API base URL, e.g. `https://api.example.com/v1`.
    pub base_url: String,
    /// Literal API key. Empty means "use an environment variable" and the
    /// key is not written at all.
    pub api_key: String,
    /// Provider package id, usually [`OPENAI_COMPATIBLE_PACKAGE`].
    pub package: String,
    /// Model id: the right half of `provider/model`.
    pub model_id: String,
    /// Display name for the model.
    pub model_name: String,
    /// Context window in tokens.
    pub context: u64,
    /// Maximum output tokens.
    pub output: u64,
}

impl ProviderDraft {
    /// A draft with the defaults the wizard starts from.
    pub fn new() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            package: OPENAI_COMPATIBLE_PACKAGE.to_string(),
            model_id: String::new(),
            model_name: String::new(),
            context: DEFAULT_CONTEXT,
            output: DEFAULT_OUTPUT,
        }
    }

    /// `provider/model`, the id the kernel lists.
    pub fn model_ref(&self) -> String {
        format!("{}/{}", self.id, self.model_id)
    }

    /// Trim every free-text field the user may have padded with spaces.
    pub fn normalize(&mut self) {
        let trim = |value: &str| value.trim().to_string();
        self.id = trim(&self.id).to_ascii_lowercase();
        self.name = trim(&self.name);
        self.base_url = trim(&self.base_url);
        self.api_key = trim(&self.api_key);
        self.model_id = trim(&self.model_id);
        self.model_name = trim(&self.model_name);
        if self.name.is_empty() {
            self.name = self.id.clone();
        }
        if self.model_name.is_empty() {
            self.model_name = self.model_id.clone();
        }
        if self.context == 0 {
            self.context = DEFAULT_CONTEXT;
        }
        if self.output == 0 {
            self.output = DEFAULT_OUTPUT;
        }
    }

    /// Reject a draft the kernel would misread, before anything is written.
    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() {
            return Err("Provider id is required (letters, digits, '.', '-', '_').".to_string());
        }
        if !self
            .id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
        {
            return Err(format!(
                "Invalid provider id `{}`: use lowercase letters, digits, '.', '-' or '_'.",
                self.id
            ));
        }
        if self.model_id.is_empty() {
            return Err("Model id is required (the name the provider's API expects).".to_string());
        }
        if self.model_id.contains('/') {
            return Err(
                "Model id must not contain '/': the provider id already supplies that part."
                    .to_string(),
            );
        }
        if self.base_url.is_empty() {
            return Err("Base URL is required, e.g. https://api.example.com/v1".to_string());
        }
        if !(self.base_url.starts_with("http://") || self.base_url.starts_with("https://")) {
            return Err("Base URL must start with http:// or https://".to_string());
        }
        if self.package.trim().is_empty() {
            return Err("Provider package is required.".to_string());
        }
        if self.context == 0 {
            return Err("Context window must be greater than zero.".to_string());
        }
        if self.output == 0 {
            return Err("Output limit must be greater than zero.".to_string());
        }
        Ok(())
    }

    /// The `providers.<id>` object the kernel understands.
    pub fn to_provider_value(&self) -> Value {
        let mut settings = Map::new();
        settings.insert("baseURL".to_string(), Value::String(self.base_url.clone()));
        if !self.api_key.is_empty() {
            settings.insert("apiKey".to_string(), Value::String(self.api_key.clone()));
        }
        let mut model = Map::new();
        model.insert("name".to_string(), Value::String(self.model_name.clone()));
        model.insert(
            "limit".to_string(),
            json!({ "context": self.context, "output": self.output }),
        );
        let mut models = Map::new();
        models.insert(self.model_id.clone(), Value::Object(model));

        let mut provider = Map::new();
        provider.insert("name".to_string(), Value::String(self.name.clone()));
        // Verified against a real kernel: without this the provider is hidden
        // unless it also has a stored credential.
        provider.insert(
            "activation".to_string(),
            Value::String("enabled".to_string()),
        );
        provider.insert("package".to_string(), Value::String(self.package.clone()));
        provider.insert("settings".to_string(), Value::Object(settings));
        provider.insert("models".to_string(), Value::Object(models));
        Value::Object(provider)
    }
}

impl Default for ProviderDraft {
    fn default() -> Self {
        Self::new()
    }
}

/// What [`save_provider`] did, for the confirmation block in the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveOutcome {
    /// File that was written.
    pub path: PathBuf,
    /// False when the file already existed and was merged into.
    pub created_file: bool,
    /// True when this write also became the default model.
    pub set_default_model: bool,
    /// Set when a higher-priority config already pins `model`, so the default
    /// written here will not be the one the kernel uses.
    pub shadowed_model_default: Option<PathBuf>,
}

/// Resolve the config directory the kernel reads, reusing the plugin
/// resolver so `/login` and `/uncensored` never disagree.
pub fn config_dir() -> Option<PathBuf> {
    crate::plugin::effective_config_dir()
}

/// Path of the file this module writes under `config_dir`.
pub fn managed_path(config_dir: &Path) -> PathBuf {
    config_dir.join(MANAGED_FILE)
}

/// Add or replace one provider, optionally making it the default model.
pub fn save_provider(draft: &ProviderDraft, make_default: bool) -> Result<SaveOutcome, String> {
    let config_dir = config_dir().ok_or_else(|| {
        "No config directory could be resolved (home directory missing).".to_string()
    })?;
    save_provider_in(&config_dir, draft, make_default)
}

/// Same as [`save_provider`] with an explicit config directory.
pub fn save_provider_in(
    config_dir: &Path,
    draft: &ProviderDraft,
    make_default: bool,
) -> Result<SaveOutcome, String> {
    let mut draft = draft.clone();
    draft.normalize();
    draft.validate()?;

    let path = managed_path(config_dir);
    let (mut root, created_file) = match fs::read_to_string(&path) {
        Ok(text) => {
            let value: Value = serde_json::from_str(&text).map_err(|error| {
                format!(
                    "{} is not plain JSON ({error}); it was left untouched. Add the provider by hand.",
                    path.display()
                )
            })?;
            match value {
                Value::Object(map) => (Value::Object(map), false),
                _ => {
                    return Err(format!(
                        "{} does not contain a JSON object; it was left untouched.",
                        path.display()
                    ));
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (json!({}), true),
        Err(error) => {
            return Err(format!("Could not read {}: {error}", path.display()));
        }
    };

    let root_map = root
        .as_object_mut()
        .ok_or_else(|| format!("{} is not a JSON object.", path.display()))?;

    let providers = root_map
        .entry("providers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let providers = providers.as_object_mut().ok_or_else(|| {
        format!(
            "`providers` in {} is not an object; it was left untouched.",
            path.display()
        )
    })?;
    // Re-running the wizard for an existing id adds a model instead of
    // dropping the ones already configured for that provider.
    match providers.get_mut(&draft.id) {
        Some(Value::Object(existing)) => merge_provider(existing, &draft),
        _ => {
            providers.insert(draft.id.clone(), draft.to_provider_value());
        }
    }

    let mut set_default_model = false;
    let mut shadowed_model_default = None;
    if make_default {
        if root_map.get("model").map(Value::is_null).unwrap_or(true) {
            root_map.insert("model".to_string(), Value::String(draft.model_ref()));
            set_default_model = true;
        }
        let shadow = config_dir.join(SHADOWING_FILE);
        if shadow.is_file() && file_defines_key(&shadow, "model") {
            shadowed_model_default = Some(shadow);
        }
    }

    let mut text = serde_json::to_string_pretty(&root)
        .map_err(|error| format!("Could not serialize config: {error}"))?;
    text.push('\n');
    fs::create_dir_all(config_dir)
        .map_err(|error| format!("Could not create {}: {error}", config_dir.display()))?;
    fs::write(&path, text)
        .map_err(|error| format!("Could not write {}: {error}", path.display()))?;

    Ok(SaveOutcome {
        path,
        created_file,
        set_default_model,
        shadowed_model_default,
    })
}

/// Merge a draft into an existing provider definition, keeping models the
/// draft does not mention and preserving any keys the draft does not own.
fn merge_provider(existing: &mut Map<String, Value>, draft: &ProviderDraft) {
    let fresh = draft.to_provider_value();
    let Some(fresh) = fresh.as_object() else {
        return;
    };
    for (key, value) in fresh {
        if key == "models" {
            let models = existing
                .entry("models".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let (Some(models), Some(new_models)) = (models.as_object_mut(), value.as_object()) {
                for (id, definition) in new_models {
                    models.insert(id.clone(), definition.clone());
                }
            }
            continue;
        }
        existing.insert(key.clone(), value.clone());
    }
}

/// Remove one provider from the managed file. Returns `false` when absent.
///
/// A `model` default that pointed at the removed provider is cleared too:
/// leaving it behind makes the kernel resolve a default that no longer exists.
pub fn remove_provider_in(config_dir: &Path, id: &str) -> Result<bool, String> {
    let path = managed_path(config_dir);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("Could not read {}: {error}", path.display())),
    };
    let mut root: Value = serde_json::from_str(&text)
        .map_err(|error| format!("{} is not plain JSON ({error}).", path.display()))?;
    let prefix = format!("{id}/");
    let mut removed = false;
    if let Some(providers) = root.get_mut("providers").and_then(Value::as_object_mut) {
        removed = providers.remove(id).is_some();
    }
    if !removed {
        return Ok(false);
    }
    let dangling_default = root
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|model| model.starts_with(&prefix));
    if dangling_default {
        if let Some(map) = root.as_object_mut() {
            map.remove("model");
        }
    }
    let mut text = serde_json::to_string_pretty(&root)
        .map_err(|error| format!("Could not serialize config: {error}"))?;
    text.push('\n');
    fs::write(&path, text)
        .map_err(|error| format!("Could not write {}: {error}", path.display()))?;
    Ok(true)
}

/// One provider as the status block shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSummary {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub models: Vec<String>,
}

/// Providers defined in the managed file, sorted by id.
pub fn configured_providers_in(config_dir: &Path) -> Vec<ProviderSummary> {
    let Some(root) = read_json(&managed_path(config_dir)) else {
        return Vec::new();
    };
    let Some(providers) = root.get("providers").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out: Vec<ProviderSummary> = providers
        .iter()
        .map(|(id, value)| ProviderSummary {
            id: id.clone(),
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_string(),
            base_url: value
                .get("settings")
                .and_then(|settings| settings.get("baseURL"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            models: value
                .get("models")
                .and_then(Value::as_object)
                .map(|models| models.keys().cloned().collect())
                .unwrap_or_default(),
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// The managed file's current `model` default, if any.
pub fn configured_default_model_in(config_dir: &Path) -> Option<String> {
    read_json(&managed_path(config_dir))?
        .get("model")?
        .as_str()
        .map(str::to_string)
}

fn read_json(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Does a (possibly JSONC) file set a non-null top-level `key`?
fn file_defines_key(path: &Path, key: &str) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = parse_jsonc(&text) else {
        return false;
    };
    value
        .get(key)
        .map(|value| !value.is_null())
        .unwrap_or(false)
}

/// Minimal JSONC reader: strips comments and trailing commas outside strings.
///
/// Only used to *inspect* a file we must not rewrite; a parse failure is
/// reported as "not defined" rather than guessed at.
pub fn parse_jsonc(text: &str) -> Result<Value, String> {
    let stripped = strip_jsonc(text);
    serde_json::from_str(&stripped).map_err(|error| error.to_string())
}

fn strip_jsonc(text: &str) -> String {
    let bytes: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
                i += 1;
            }
            '/' if bytes.get(i + 1) == Some(&'/') => {
                while i < bytes.len() && bytes[i] != '\n' {
                    i += 1;
                }
            }
            '/' if bytes.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == '*' && bytes.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    strip_trailing_commas(&out)
}

fn strip_trailing_commas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for (index, &c) in chars.iter().enumerate() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            continue;
        }
        if c == ',' {
            let next = chars[index + 1..].iter().find(|n| !n.is_whitespace());
            if matches!(next, Some('}') | Some(']')) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let mut path = std::env::temp_dir();
            let unique = format!(
                "{prefix}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            );
            path.push(unique);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn draft() -> ProviderDraft {
        ProviderDraft {
            id: "gateway".to_string(),
            name: "My Gateway".to_string(),
            base_url: "https://gateway.example.com/v1".to_string(),
            api_key: "sk-test".to_string(),
            package: OPENAI_COMPATIBLE_PACKAGE.to_string(),
            model_id: "gpt-oss-120b".to_string(),
            model_name: "GPT OSS 120B".to_string(),
            context: 131072,
            output: 8192,
        }
    }

    #[test]
    fn creates_managed_file_with_activation_enabled() {
        let dir = TempDir::new("providers-create");
        let outcome = save_provider_in(dir.path(), &draft(), true).unwrap();
        assert!(outcome.created_file);
        assert!(outcome.set_default_model);

        let text = std::fs::read_to_string(outcome.path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let provider = &value["providers"]["gateway"];
        assert_eq!(provider["activation"], "enabled");
        assert_eq!(provider["package"], OPENAI_COMPATIBLE_PACKAGE);
        assert_eq!(
            provider["settings"]["baseURL"],
            "https://gateway.example.com/v1"
        );
        assert_eq!(provider["settings"]["apiKey"], "sk-test");
        assert_eq!(value["model"], "gateway/gpt-oss-120b");
    }

    #[test]
    fn merges_into_existing_file_and_keeps_other_keys() {
        let dir = TempDir::new("providers-merge");
        let path = managed_path(dir.path());
        std::fs::write(
            &path,
            r#"{"model":"keep/me","theme":"dark","providers":{}}"#,
        )
        .unwrap();

        let outcome = save_provider_in(dir.path(), &draft(), true).unwrap();
        assert!(!outcome.created_file);
        // A pre-existing default is never silently replaced.
        assert!(!outcome.set_default_model);

        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(outcome.path).unwrap()).unwrap();
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["model"], "keep/me");
        assert_eq!(value["providers"]["gateway"]["name"], "My Gateway");
    }

    #[test]
    fn empty_api_key_is_not_written() {
        let dir = TempDir::new("providers-nokey");
        let mut d = draft();
        d.api_key.clear();
        save_provider_in(dir.path(), &d, false).unwrap();
        let value = read_json(&managed_path(dir.path())).unwrap();
        assert!(
            value["providers"]["gateway"]["settings"]
                .get("apiKey")
                .is_none()
        );
    }

    #[test]
    fn invalid_draft_leaves_file_untouched() {
        let dir = TempDir::new("providers-invalid");
        let path = managed_path(dir.path());
        std::fs::write(&path, "{\"keep\":true}\n").unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let mut d = draft();
        d.base_url = "gateway.example.com".to_string();
        let error = save_provider_in(dir.path(), &d, true).unwrap_err();
        assert!(error.contains("http"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn id_is_lowercased_and_names_defaulted() {
        let dir = TempDir::new("providers-case");
        let mut d = draft();
        d.id = "  My-Gateway  ".to_string();
        d.name.clear();
        d.model_name.clear();
        save_provider_in(dir.path(), &d, false).unwrap();
        let value = read_json(&managed_path(dir.path())).unwrap();
        let provider = &value["providers"]["my-gateway"];
        assert_eq!(provider["name"], "my-gateway");
        assert_eq!(provider["models"]["gpt-oss-120b"]["name"], "gpt-oss-120b");
    }

    #[test]
    fn malformed_file_is_reported_not_overwritten() {
        let dir = TempDir::new("providers-badjson");
        let path = managed_path(dir.path());
        std::fs::write(&path, "{ not json").unwrap();
        let error = save_provider_in(dir.path(), &draft(), false).unwrap_err();
        assert!(error.contains("not plain JSON"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn shadowing_jsonc_model_default_is_reported() {
        let dir = TempDir::new("providers-shadow");
        std::fs::write(
            dir.path().join(SHADOWING_FILE),
            "{\n  // pinned by hand\n  \"model\": \"other/model\",\n}\n",
        )
        .unwrap();
        let outcome = save_provider_in(dir.path(), &draft(), true).unwrap();
        assert_eq!(
            outcome.shadowed_model_default.as_deref(),
            Some(dir.path().join(SHADOWING_FILE).as_path())
        );
    }

    #[test]
    fn rerunning_for_the_same_id_keeps_earlier_models() {
        let dir = TempDir::new("providers-rerun");
        save_provider_in(dir.path(), &draft(), false).unwrap();
        let mut second = draft();
        second.model_id = "second-model".to_string();
        second.model_name = "Second".to_string();
        save_provider_in(dir.path(), &second, false).unwrap();
        let value = read_json(&managed_path(dir.path())).unwrap();
        let models = value["providers"]["gateway"]["models"].as_object().unwrap();
        assert!(models.contains_key("gpt-oss-120b"));
        assert!(models.contains_key("second-model"));
    }

    #[test]
    fn lists_and_removes_providers() {
        let dir = TempDir::new("providers-list");
        save_provider_in(dir.path(), &draft(), true).unwrap();
        let listed = configured_providers_in(dir.path());
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "gateway");
        assert_eq!(listed[0].models, vec!["gpt-oss-120b".to_string()]);
        assert_eq!(
            configured_default_model_in(dir.path()).as_deref(),
            Some("gateway/gpt-oss-120b")
        );

        assert!(remove_provider_in(dir.path(), "gateway").unwrap());
        assert!(!remove_provider_in(dir.path(), "gateway").unwrap());
        assert!(configured_providers_in(dir.path()).is_empty());
    }

    #[test]
    fn removing_a_provider_clears_a_default_that_pointed_at_it() {
        let dir = TempDir::new("providers-remove-default");
        save_provider_in(dir.path(), &draft(), true).unwrap();
        assert!(remove_provider_in(dir.path(), "gateway").unwrap());
        let value = read_json(&managed_path(dir.path())).unwrap();
        assert!(value.get("model").is_none(), "{value}");
    }

    #[test]
    fn removing_a_provider_keeps_an_unrelated_default() {
        let dir = TempDir::new("providers-remove-other");
        let path = managed_path(dir.path());
        std::fs::write(
            &path,
            r#"{"model":"other/model","providers":{"gateway":{}}}"#,
        )
        .unwrap();
        assert!(remove_provider_in(dir.path(), "gateway").unwrap());
        let value = read_json(&path).unwrap();
        assert_eq!(value["model"], "other/model");
    }

    #[test]
    fn jsonc_comments_and_trailing_commas_are_tolerated() {
        let value = parse_jsonc(
            r#"{
              // line comment with "quotes"
              "model": "a/b", /* block */
              "providers": { "x": { "name": "X", }, },
            }"#,
        )
        .unwrap();
        assert_eq!(value["model"], "a/b");
        assert_eq!(value["providers"]["x"]["name"], "X");
    }

    #[test]
    fn comments_inside_strings_survive() {
        let value = parse_jsonc(r#"{"url":"https://x//y","note":"/* not a comment */"}"#).unwrap();
        assert_eq!(value["url"], "https://x//y");
        assert_eq!(value["note"], "/* not a comment */");
    }
}
