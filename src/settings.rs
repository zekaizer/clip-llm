//! In-app settings panel model: the live config read into an editable form,
//! validated into a [`SettingsPatch`], and written back into `config.toml` in
//! place (comments and unrelated keys untouched — see ADR-0001).

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::{ProcessMode, ThinkingMode};

/// Accepted range for `[hotkey].double_tap_timeout_ms`.
pub const DOUBLE_TAP_MS_RANGE: std::ops::RangeInclusive<u64> = 100..=2000;

/// API flavor of a model profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    GrokOauth,
}

impl Provider {
    pub const ALL: &[Self] = &[Self::OpenAi, Self::GrokOauth];

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI-compatible",
            Self::GrokOauth => "Grok (CLI sign-in)",
        }
    }

    /// The `provider` key value.
    pub fn key(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::GrokOauth => "grok-oauth",
        }
    }

    /// Unknown/absent = `openai`, matching the client's default.
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some("grok-oauth") => Self::GrokOauth,
            _ => Self::OpenAi,
        }
    }
}

/// One model profile as edited in the panel; strings until validated.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileForm {
    pub name: String,
    pub provider: Provider,
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub auth_file: String,
    pub max_tokens: String,
    pub token_budget: String,
    /// `thinking_control` key; empty = auto (probe on first use).
    pub thinking_control: String,
    /// Lives in `[api]` (the `CLIP_LLM_*`-overridable profile) rather than in
    /// a `[[models]]` entry.
    pub from_api_section: bool,
}

/// Selectable `thinking_control` values: (key, label, explanation).
pub const THINKING_CONTROLS: &[(&str, &str, &str)] = &[
    ("", "Auto", "Probe once: try reasoning_effort, chat_template_kwargs, then /no_think and keep the first one that actually stops reasoning"),
    ("reasoning_effort", "reasoning_effort", "Send reasoning_effort = \"none\" (LM Studio, Groq, OpenAI)"),
    ("chat_template_kwargs", "kwargs", "Send chat_template_kwargs.enable_thinking = false (vLLM Qwen3)"),
    ("prompt_tag", "/no_think", "Prefix the system prompt with /no_think (Qwen)"),
    ("none", "None", "Never try to switch thinking off (always-on reasoning models)"),
];

impl ProfileForm {
    pub fn from_spec(spec: &crate::config::ModelSpec) -> Self {
        let text = |v: &Option<String>| v.clone().unwrap_or_default();
        Self {
            name: spec.name.clone(),
            provider: Provider::parse(spec.provider.as_deref()),
            endpoint: text(&spec.endpoint),
            model: text(&spec.model),
            api_key: text(&spec.api_key),
            auth_file: text(&spec.auth_file),
            max_tokens: spec.max_tokens.map(|v| v.to_string()).unwrap_or_default(),
            token_budget: spec.token_budget.map(|v| v.to_string()).unwrap_or_default(),
            thinking_control: spec
                .thinking_control
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty() && *v != "auto")
                .unwrap_or("")
                .to_string(),
            from_api_section: spec.from_api_section,
        }
    }

    /// A new, empty `[[models]]` entry.
    pub fn blank() -> Self {
        Self {
            name: String::new(),
            provider: Provider::OpenAi,
            endpoint: String::new(),
            model: String::new(),
            api_key: String::new(),
            auth_file: String::new(),
            max_tokens: String::new(),
            token_budget: String::new(),
            thinking_control: String::new(),
            from_api_section: false,
        }
    }

    /// Validate into a spec; `Err` names the field.
    pub fn to_spec(&self) -> Result<crate::config::ModelSpec, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Every profile needs a name.".to_string());
        }
        let non_empty = |v: &str| Some(v.trim()).filter(|t| !t.is_empty()).map(str::to_string);
        // The [api] profile may leave fields empty: CLIP_LLM_* variables fill
        // them at startup, so only [[models]] entries are checked for completeness.
        if self.from_api_section {
            return Ok(crate::config::ModelSpec {
                name: name.to_string(),
                provider: Some(self.provider.key().to_string()),
                endpoint: non_empty(&self.endpoint),
                model: non_empty(&self.model),
                api_key: non_empty(&self.api_key),
                auth_file: non_empty(&self.auth_file),
                headers: Default::default(),
                max_tokens: None,
                token_budget: None,
                thinking_control: non_empty(&self.thinking_control),
                from_api_section: true,
            });
        }
        let model = self.model.trim();
        if model.is_empty() {
            return Err(format!("Profile \"{name}\": the model is required."));
        }
        let number = |v: &str, key: &str| -> Result<Option<u32>, String> {
            let t = v.trim();
            if t.is_empty() {
                return Ok(None);
            }
            t.parse::<u32>()
                .ok()
                .filter(|n| *n > 0)
                .map(Some)
                .ok_or_else(|| format!("Profile \"{name}\": {key} must be a positive number."))
        };
        let (endpoint, api_key) = match self.provider {
            Provider::OpenAi => {
                let endpoint = non_empty(&self.endpoint)
                    .ok_or_else(|| format!("Profile \"{name}\": the endpoint URL is required."))?;
                let api_key = non_empty(&self.api_key)
                    .ok_or_else(|| format!("Profile \"{name}\": the API key is required."))?;
                (Some(endpoint), Some(api_key))
            }
            // Grok uses the CLI's OAuth session; a stray endpoint would silently
            // redirect requests, so neither is kept.
            Provider::GrokOauth => (None, None),
        };
        Ok(crate::config::ModelSpec {
            name: name.to_string(),
            provider: Some(self.provider.key().to_string()),
            endpoint,
            model: Some(model.to_string()),
            api_key,
            auth_file: non_empty(&self.auth_file),
            headers: Default::default(),
            max_tokens: number(&self.max_tokens, "max_tokens")?,
            token_budget: number(&self.token_budget, "token_budget")?,
            thinking_control: non_empty(&self.thinking_control),
            from_api_section: self.from_api_section,
        })
    }

    /// One-line description for the profile list ("openai · qwen/qwen3-32b").
    pub fn summary(&self) -> String {
        let model = if self.model.trim().is_empty() { "(no model)" } else { self.model.trim() };
        format!("{} \u{b7} {model}", self.provider.key())
    }
}

/// Editable state behind the settings panel. Text fields stay strings until
/// [`SettingsForm::to_patch`] validates them.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsForm {
    pub primary: String,
    pub secondary: String,
    /// Mode selected at startup (= first tab).
    pub default_mode: ProcessMode,
    pub double_tap_ms: String,
    pub single_tap_pinned: bool,
    pub double_tap_pinned: bool,
    /// Per-mode thinking override in display order; `None` = built-in default.
    pub thinking: Vec<(ProcessMode, Option<ThinkingMode>)>,
    /// Model profiles in config order and the one to start with.
    pub profiles: Vec<ProfileForm>,
    pub default_model: usize,
    /// Profile whose editor is expanded, if any.
    pub editing: Option<usize>,
    /// Show API keys in clear text.
    pub show_key: bool,
    /// Validation/save error shown in the panel.
    pub error: Option<String>,
    /// Outcome of the last save, shown in the panel until the next edit.
    pub notice: Option<String>,
}

/// Languages offered in the picker; anything else is typed in.
pub const COMMON_LANGUAGES: &[&str] = &[
    "Korean", "English", "Japanese", "Chinese", "German", "French", "Spanish", "Portuguese",
    "Vietnamese", "Thai", "Indonesian",
];

/// Validated settings, ready to write.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsPatch {
    pub primary: String,
    pub secondary: String,
    /// Full tab order: the default mode first, the rest in current order.
    pub tabs: Vec<ProcessMode>,
    pub double_tap_ms: u64,
    pub single_tap_pinned: bool,
    pub double_tap_pinned: bool,
    pub thinking: Vec<(ProcessMode, Option<ThinkingMode>)>,
    /// `None` when there is only one profile (key left alone).
    pub default_model: Option<String>,
    /// Every profile, config order: the `[api]` one (if any) plus `[[models]]`.
    pub profiles: Vec<crate::config::ModelSpec>,
}

impl SettingsForm {
    /// Snapshot of the live config for editing. `active_model` names the
    /// profile in use so it is preselected as the startup default.
    pub fn from_config(config: &Config, active_model: Option<&str>) -> Self {
        let tabs = config.ui_tab_order();
        let profiles: Vec<ProfileForm> = config
            .model_specs()
            .unwrap_or_default()
            .iter()
            .map(ProfileForm::from_spec)
            .collect();
        let default_model = active_model
            .and_then(|name| profiles.iter().position(|p| p.name == name))
            .unwrap_or(0);
        Self {
            primary: config.primary_lang().to_string(),
            secondary: config.secondary_lang().to_string(),
            default_mode: tabs.first().copied().unwrap_or_default(),
            double_tap_ms: config
                .hotkey_double_tap_timeout_ms()
                .map(|ms| ms.to_string())
                .unwrap_or_else(|| crate::hotkey::DEFAULT_DOUBLE_TAP_TIMEOUT.as_millis().to_string()),
            single_tap_pinned: config.ui_single_tap_pinned(),
            double_tap_pinned: config.ui_double_tap_pinned(),
            thinking: tabs.iter().map(|&m| (m, config.mode_default_thinking(m))).collect(),
            profiles,
            default_model,
            editing: None,
            show_key: false,
            error: None,
            notice: None,
        }
    }

    /// Profile names in config order (the panel's model pills).
    pub fn profile_names(&self) -> Vec<String> {
        self.profiles.iter().map(|p| p.name.trim().to_string()).collect()
    }

    /// Validate the form; `Err` is the message to show next to the Save button.
    pub fn to_patch(&self) -> Result<SettingsPatch, String> {
        let primary = self.primary.trim();
        let secondary = self.secondary.trim();
        if primary.is_empty() || secondary.is_empty() {
            return Err("Both language names are required.".to_string());
        }
        if primary.eq_ignore_ascii_case(secondary) {
            return Err("The two languages must differ.".to_string());
        }
        let double_tap_ms = self
            .double_tap_ms
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|ms| DOUBLE_TAP_MS_RANGE.contains(ms))
            .ok_or_else(|| {
                format!(
                    "Double-tap window must be {}\u{2013}{} ms.",
                    DOUBLE_TAP_MS_RANGE.start(),
                    DOUBLE_TAP_MS_RANGE.end()
                )
            })?;
        let mut tabs = vec![self.default_mode];
        tabs.extend(self.thinking.iter().map(|(m, _)| *m).filter(|m| *m != self.default_mode));
        if self.profiles.is_empty() {
            return Err("At least one model profile is required.".to_string());
        }
        let mut profiles = Vec::with_capacity(self.profiles.len());
        for profile in &self.profiles {
            let spec = profile.to_spec()?;
            if profiles.iter().any(|p: &crate::config::ModelSpec| p.name == spec.name) {
                return Err(format!("Two profiles are named \"{}\".", spec.name));
            }
            profiles.push(spec);
        }
        let default_model = if self.profiles.len() > 1 {
            self.profiles.get(self.default_model).map(|p| p.name.trim().to_string())
        } else {
            None
        };
        Ok(SettingsPatch {
            primary: primary.to_string(),
            secondary: secondary.to_string(),
            tabs,
            double_tap_ms,
            single_tap_pinned: self.single_tap_pinned,
            double_tap_pinned: self.double_tap_pinned,
            thinking: self.thinking.clone(),
            default_model,
            profiles,
        })
    }
}

/// TOML key of a mode's section (`[translate]`, ...).
pub fn mode_key(mode: ProcessMode) -> String {
    mode.label().to_ascii_lowercase()
}

/// Set exactly the keys the panel owns in `doc`, leaving comments, ordering
/// and every other key as they were.
pub fn apply_patch(doc: &mut toml_edit::Document, patch: &SettingsPatch) {
    let languages = section(doc, "languages");
    set_scalar(languages, "primary", patch.primary.as_str());
    set_scalar(languages, "secondary", patch.secondary.as_str());

    let ui = section(doc, "ui");
    let mut tabs = toml_edit::Array::new();
    for mode in &patch.tabs {
        tabs.push(mode_key(*mode));
    }
    set_scalar(ui, "tabs", toml_edit::Value::Array(tabs));
    set_scalar(ui, "single_tap_pinned", patch.single_tap_pinned);
    set_scalar(ui, "double_tap_pinned", patch.double_tap_pinned);
    if let Some(name) = &patch.default_model {
        set_scalar(ui, "default_model", name.as_str());
    }

    let hotkey = section(doc, "hotkey");
    set_scalar(hotkey, "double_tap_timeout_ms", patch.double_tap_ms as i64);

    apply_profiles(doc, &patch.profiles);

    for (mode, thinking) in &patch.thinking {
        let key = mode_key(*mode);
        match thinking {
            Some(t) => {
                let name = match t {
                    ThinkingMode::Think => "think",
                    ThinkingMode::NoThink => "no_think",
                };
                set_scalar(section(doc, &key), "thinking", name);
            }
            // Built-in default: drop the override, and the section too when
            // nothing else lives in it, so no empty `[mode]` header is left.
            None => {
                let now_empty = doc
                    .get_mut(&key)
                    .and_then(toml_edit::Item::as_table_mut)
                    .map(|t| {
                        t.remove("thinking");
                        t.is_empty()
                    });
                if now_empty == Some(true) {
                    doc.remove(&key);
                }
            }
        }
    }
}

/// Keys of `[api]` that make up its model profile (the rest of the section —
/// `streaming`, `headers` — is not profile data and is left alone).
const API_PROFILE_KEYS: [&str; 6] =
    ["provider", "endpoint", "model", "api_key", "auth_file", "thinking_control"];

/// Write the profile list: the `[api]` profile (if present) in place, every
/// other profile as a rebuilt `[[models]]` array (their own comments are not
/// preserved — the panel owns those tables).
fn apply_profiles(doc: &mut toml_edit::Document, profiles: &[crate::config::ModelSpec]) {
    // An empty list is "not edited" (a patch never legitimately removes every
    // profile — validation requires at least one).
    if profiles.is_empty() {
        return;
    }
    match profiles.iter().find(|p| p.from_api_section) {
        Some(api) => {
            let table = section(doc, "api");
            set_optional(table, "provider", api.provider.as_deref());
            set_optional(table, "endpoint", api.endpoint.as_deref());
            set_optional(table, "model", api.model.as_deref());
            set_optional(table, "api_key", api.api_key.as_deref());
            set_optional(table, "auth_file", api.auth_file.as_deref());
            set_optional(table, "thinking_control", api.thinking_control.as_deref());
        }
        None => {
            if let Some(table) = doc.get_mut("api").and_then(toml_edit::Item::as_table_mut) {
                for key in API_PROFILE_KEYS {
                    table.remove(key);
                }
            }
        }
    }

    let others: Vec<&crate::config::ModelSpec> =
        profiles.iter().filter(|p| !p.from_api_section).collect();
    if others.is_empty() {
        doc.remove("models");
        return;
    }
    let mut array = toml_edit::ArrayOfTables::new();
    for spec in others {
        let mut table = toml_edit::Table::new();
        table.insert("name", toml_edit::value(spec.name.as_str()));
        if let Some(v) = &spec.provider {
            table.insert("provider", toml_edit::value(v.as_str()));
        }
        if let Some(v) = &spec.endpoint {
            table.insert("endpoint", toml_edit::value(v.as_str()));
        }
        if let Some(v) = &spec.model {
            table.insert("model", toml_edit::value(v.as_str()));
        }
        if let Some(v) = &spec.api_key {
            table.insert("api_key", toml_edit::value(v.as_str()));
        }
        if let Some(v) = &spec.auth_file {
            table.insert("auth_file", toml_edit::value(v.as_str()));
        }
        if let Some(v) = spec.max_tokens {
            table.insert("max_tokens", toml_edit::value(i64::from(v)));
        }
        if let Some(v) = spec.token_budget {
            table.insert("token_budget", toml_edit::value(i64::from(v)));
        }
        if let Some(v) = &spec.thinking_control {
            table.insert("thinking_control", toml_edit::value(v.as_str()));
        }
        if !spec.headers.is_empty() {
            let mut headers = toml_edit::Table::new();
            for (k, v) in &spec.headers {
                headers.insert(k, toml_edit::value(v.as_str()));
            }
            table.insert("headers", toml_edit::Item::Table(headers));
        }
        array.push(table);
    }
    doc.insert("models", toml_edit::Item::ArrayOfTables(array));
}

/// Set a string key, or remove it when the value is `None`.
fn set_optional(table: &mut toml_edit::Table, key: &str, value: Option<&str>) {
    match value {
        Some(v) => set_scalar(table, key, v),
        None => {
            table.remove(key);
        }
    }
}

/// The named top-level table, created (non-implicit, so its header prints)
/// when absent. A non-table item under that key is replaced.
fn section<'a>(doc: &'a mut toml_edit::Document, name: &str) -> &'a mut toml_edit::Table {
    let item = doc.entry(name).or_insert(toml_edit::table());
    if !item.is_table() {
        *item = toml_edit::table();
    }
    item.as_table_mut().expect("just ensured a table")
}

/// Replace a scalar keeping the old value's decor (spacing and any trailing
/// comment); insert plainly when the key is new.
fn set_scalar<V: Into<toml_edit::Value>>(table: &mut toml_edit::Table, key: &str, value: V) {
    let mut new: toml_edit::Value = value.into();
    match table.get_mut(key).and_then(toml_edit::Item::as_value_mut) {
        Some(existing) => {
            *new.decor_mut() = existing.decor().clone();
            *existing = new;
        }
        None => {
            table.insert(key, toml_edit::Item::Value(new));
        }
    }
}

/// Apply `patch` to the file at `path` (which must exist) and write it back
/// atomically. Returns the path on success.
pub fn save_to(path: &Path, patch: &SettingsPatch) -> Result<PathBuf, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    // The parse error's Display renders the offending line — which may hold an
    // api_key — so only a generic message leaves this function.
    let mut doc: toml_edit::Document = text
        .parse()
        .map_err(|_| format!("{} is not valid TOML; fix it by hand first", path.display()))?;
    apply_patch(&mut doc, patch);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string())
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot replace {}: {e}", path.display())
    })?;
    Ok(path.to_path_buf())
}

/// Apply `patch` to the active config file, creating the starter file first
/// when none exists yet.
pub fn save(patch: &SettingsPatch) -> Result<PathBuf, String> {
    let path = crate::config::ensure_config_file().ok_or("no writable config path")?;
    save_to(&path, patch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_default_thinking() -> Vec<(ProcessMode, Option<ThinkingMode>)> {
        ProcessMode::ALL.iter().map(|&m| (m, None)).collect()
    }

    fn sample_patch() -> SettingsPatch {
        SettingsPatch {
            primary: "German".into(),
            secondary: "English".into(),
            tabs: vec![
                ProcessMode::Summarize,
                ProcessMode::Translate,
                ProcessMode::Rephrase,
                ProcessMode::Explain,
                ProcessMode::Transcribe,
            ],
            double_tap_ms: 300,
            single_tap_pinned: true,
            double_tap_pinned: false,
            thinking: vec![
                (ProcessMode::Translate, Some(ThinkingMode::Think)),
                (ProcessMode::Rephrase, None),
                (ProcessMode::Summarize, None),
                (ProcessMode::Explain, Some(ThinkingMode::NoThink)),
                (ProcessMode::Transcribe, None),
            ],
            default_model: Some("groq".into()),
            profiles: vec![],
        }
    }

    fn openai_profile(name: &str) -> ProfileForm {
        ProfileForm {
            name: name.into(),
            provider: Provider::OpenAi,
            endpoint: "https://api.groq.com/openai/v1".into(),
            model: "qwen/qwen3-32b".into(),
            api_key: "gsk".into(),
            auth_file: String::new(),
            max_tokens: "40960".into(),
            token_budget: "6000".into(),
            thinking_control: String::new(),
            from_api_section: false,
        }
    }

    const DOC: &str = r#"# top comment stays
[api]
model = "m" # keep me

[languages]
primary   = "Korean"   # trailing comment survives
secondary = "English"

[summarize]
prompt = "P"
thinking = "think"
"#;

    #[test]
    fn from_config_reads_live_values() {
        let cfg: Config = toml::from_str(
            "[languages]\nprimary = \"Japanese\"\n[hotkey]\ndouble_tap_timeout_ms = 400\n[ui]\nsingle_tap_pinned = true\n[summarize]\nthinking = \"no_think\"\n",
        )
        .unwrap();
        let form = SettingsForm::from_config(&cfg, None);
        assert_eq!(form.primary, "Japanese");
        assert_eq!(form.secondary, "English");
        assert_eq!(form.double_tap_ms, "400");
        assert!(form.single_tap_pinned);
        assert!(!form.double_tap_pinned);
        assert_eq!(form.default_model, 0);
        assert_eq!(form.thinking.len(), ProcessMode::ALL.len());
        assert!(form
            .thinking
            .contains(&(ProcessMode::Summarize, Some(ThinkingMode::NoThink))));
        assert!(form.thinking.contains(&(ProcessMode::Translate, None)));
        assert_eq!(form.error, None);
    }

    #[test]
    fn from_config_default_double_tap_is_the_hotkey_default() {
        let form = SettingsForm::from_config(&Config::default(), None);
        assert_eq!(form.double_tap_ms, crate::hotkey::DEFAULT_DOUBLE_TAP_TIMEOUT.as_millis().to_string());
        assert_eq!(form.primary, "Korean");
    }

    #[test]
    fn to_patch_puts_default_mode_first_and_keeps_the_rest_in_order() {
        let mut form = SettingsForm::from_config(&two_profiles(), Some("a"));
        form.default_mode = ProcessMode::Explain;
        form.default_model = 1;
        let patch = form.to_patch().unwrap();
        assert_eq!(patch.tabs[0], ProcessMode::Explain);
        assert_eq!(patch.tabs.len(), ProcessMode::ALL.len());
        let rest: Vec<_> = patch.tabs[1..].to_vec();
        assert_eq!(
            rest,
            vec![ProcessMode::Translate, ProcessMode::Rephrase, ProcessMode::Summarize, ProcessMode::Transcribe]
        );
        assert_eq!(patch.default_model.as_deref(), Some("a"));
        assert_eq!(u128::from(patch.double_tap_ms), crate::hotkey::DEFAULT_DOUBLE_TAP_TIMEOUT.as_millis());
    }

    #[test]
    fn to_patch_single_profile_leaves_default_model_alone() {
        let form = SettingsForm::from_config(&Config::default(), None);
        assert_eq!(form.profiles.len(), 1);
        assert_eq!(form.to_patch().unwrap().default_model, None);
    }

    #[test]
    fn to_patch_validates_languages_and_timeout() {
        let mut form = SettingsForm::from_config(&Config::default(), None);
        form.primary = "  ".into();
        assert!(form.to_patch().unwrap_err().contains("language"));
        form.primary = "english".into();
        form.secondary = "English".into();
        assert!(form.to_patch().unwrap_err().contains("differ"));
        form.primary = "Korean".into();
        form.double_tap_ms = "abc".into();
        assert!(form.to_patch().unwrap_err().contains("100"));
        form.double_tap_ms = "50".into();
        assert!(form.to_patch().unwrap_err().contains("100"));
        form.double_tap_ms = " 250 ".into();
        assert_eq!(form.to_patch().unwrap().double_tap_ms, 250);
    }

    fn two_profiles() -> Config {
        toml::from_str(
            "[api]\nprovider = \"grok-oauth\"\nmodel = \"grok-4.3\"\n[[models]]\nname = \"a\"\nendpoint = \"http://h/v1\"\nmodel = \"m\"\napi_key = \"k\"\n[[models]]\nname = \"b\"\nendpoint = \"http://h/v1\"\nmodel = \"m2\"\napi_key = \"k2\"\ntoken_budget = 6000\n",
        )
        .unwrap()
    }

    // --- profiles ---

    #[test]
    fn from_config_lists_every_profile_and_preselects_the_active_one() {
        let form = SettingsForm::from_config(&two_profiles(), Some("b"));
        assert_eq!(form.profile_names(), vec!["grok-4.3", "a", "b"]);
        assert_eq!(form.default_model, 2);
        assert!(form.profiles[0].from_api_section);
        assert_eq!(form.profiles[0].provider, Provider::GrokOauth);
        assert_eq!(form.profiles[0].model, "grok-4.3");
        assert_eq!(form.profiles[2].token_budget, "6000");
        assert_eq!(form.profiles[2].max_tokens, "");
        assert_eq!(form.profiles[2].api_key, "k2");
        assert_eq!(form.profiles[1].summary(), "openai \u{b7} m");
        assert_eq!(form.editing, None);
        // Unknown active name falls back to the first profile.
        assert_eq!(SettingsForm::from_config(&two_profiles(), Some("zzz")).default_model, 0);
    }

    #[test]
    fn profile_to_spec_validates_per_provider() {
        let ok = openai_profile("groq").to_spec().unwrap();
        assert_eq!(ok.name, "groq");
        assert_eq!(ok.provider.as_deref(), Some("openai"));
        assert_eq!(ok.endpoint.as_deref(), Some("https://api.groq.com/openai/v1"));
        assert_eq!(ok.api_key.as_deref(), Some("gsk"));
        assert_eq!(ok.max_tokens, Some(40960));
        assert_eq!(ok.token_budget, Some(6000));
        assert!(!ok.from_api_section);

        let mut p = openai_profile("x");
        p.api_key = "  ".into();
        assert!(p.to_spec().unwrap_err().contains("API key"));
        let mut p = openai_profile("x");
        p.endpoint = String::new();
        assert!(p.to_spec().unwrap_err().contains("endpoint"));
        let mut p = openai_profile("x");
        p.model = String::new();
        assert!(p.to_spec().unwrap_err().contains("model"));
        let mut p = openai_profile("x");
        p.max_tokens = "lots".into();
        assert!(p.to_spec().unwrap_err().contains("max_tokens"));
        let mut p = openai_profile("  ");
        p.name = "  ".into();
        assert!(p.to_spec().unwrap_err().contains("name"));

        // Grok needs only a model; key/endpoint are dropped, auth_file kept.
        let mut g = openai_profile("grok");
        g.provider = Provider::GrokOauth;
        g.model = "grok-4.3".into();
        g.auth_file = "/x/auth.json".into();
        let spec = g.to_spec().unwrap();
        assert_eq!(spec.provider.as_deref(), Some("grok-oauth"));
        assert_eq!(spec.endpoint, None);
        assert_eq!(spec.api_key, None);
        assert_eq!(spec.auth_file.as_deref(), Some("/x/auth.json"));
    }

    #[test]
    fn thinking_control_round_trips_through_form_and_file() {
        let mut p = openai_profile("g");
        p.thinking_control = "reasoning_effort".into();
        let spec = p.to_spec().unwrap();
        assert_eq!(spec.thinking_control.as_deref(), Some("reasoning_effort"));
        assert_eq!(ProfileForm::from_spec(&spec).thinking_control, "reasoning_effort");
        let mut auto = spec.clone();
        auto.thinking_control = Some("auto".into());
        assert_eq!(ProfileForm::from_spec(&auto).thinking_control, "", "auto shows as the empty default");

        let mut doc: toml_edit::Document = "[api]\nmodel = \"m\"\n".parse().unwrap();
        let mut patch = sample_patch();
        let mut api = openai_profile("api").to_spec().unwrap();
        api.from_api_section = true;
        api.thinking_control = Some("none".into());
        patch.profiles = vec![api, spec];
        apply_patch(&mut doc, &patch);
        let cfg: Config = toml::from_str(&doc.to_string()).unwrap();
        let specs = cfg.model_specs().unwrap();
        assert_eq!(specs[0].thinking_control.as_deref(), Some("none"));
        assert_eq!(specs[1].thinking_control.as_deref(), Some("reasoning_effort"));
    }

    #[test]
    fn to_patch_carries_profiles_and_rejects_duplicates_or_none() {
        let mut form = SettingsForm::from_config(&two_profiles(), None);
        let patch = form.to_patch().unwrap();
        assert_eq!(patch.profiles.len(), 3);
        assert!(patch.profiles[0].from_api_section);
        assert_eq!(patch.profiles[2].name, "b");

        form.profiles[2].name = "a".into();
        assert!(form.to_patch().unwrap_err().contains("\"a\""));

        form.profiles.clear();
        assert!(form.to_patch().unwrap_err().contains("profile"));
    }

    #[test]
    fn apply_patch_rewrites_profiles() {
        let mut doc: toml_edit::Document = r#"[api]
provider = "grok-oauth"   # keep this comment
model = "grok-4.3"
streaming = true

[[models]]
name = "old"
model = "gone"
"#
        .parse()
        .unwrap();
        let mut patch = sample_patch();
        let form = SettingsForm::from_config(&two_profiles(), None);
        let mut profiles: Vec<_> = form.profiles.iter().map(|p| p.to_spec().unwrap()).collect();
        profiles[0].model = Some("grok-4.5".into());
        profiles.pop(); // drop "b"
        patch.profiles = profiles;
        apply_patch(&mut doc, &patch);
        let out = doc.to_string();
        assert!(out.contains("# keep this comment"), "{out}");
        assert!(out.contains("streaming = true"), "{out}");
        assert!(!out.contains("gone"), "{out}");
        let cfg: Config = toml::from_str(&out).unwrap();
        let specs = cfg.model_specs().unwrap();
        let names: Vec<_> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["grok-4.5", "a"]);
        assert_eq!(specs[0].provider.as_deref(), Some("grok-oauth"));
        assert_eq!(specs[1].endpoint.as_deref(), Some("http://h/v1"));
        assert_eq!(specs[1].api_key.as_deref(), Some("k"));
    }

    #[test]
    fn apply_patch_can_move_everything_out_of_api_section() {
        let mut doc: toml_edit::Document =
            "[api]\nendpoint = \"http://h/v1\"\nmodel = \"m\"\napi_key = \"k\"\nstreaming = false\n"
                .parse()
                .unwrap();
        let mut patch = sample_patch();
        patch.profiles = vec![openai_profile("only").to_spec().unwrap()];
        apply_patch(&mut doc, &patch);
        let cfg: Config = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(cfg.api_model(), None, "[api] profile removed");
        assert_eq!(cfg.api_streaming(), Some(false), "non-profile [api] keys survive");
        let names: Vec<_> = cfg.model_specs().unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["only"]);
    }

    #[test]
    fn apply_patch_edits_in_place_and_preserves_everything_else() {
        let mut doc: toml_edit::Document = DOC.parse().unwrap();
        apply_patch(&mut doc, &sample_patch());
        let out = doc.to_string();
        assert!(out.contains("# top comment stays"), "{out}");
        assert!(out.contains("model = \"m\" # keep me"), "{out}");
        assert!(out.contains("primary   = \"German\"   # trailing comment survives"), "{out}");
        assert!(out.contains("prompt = \"P\""), "{out}");

        let cfg: Config = toml::from_str(&out).expect("edited file must stay valid TOML");
        assert_eq!(cfg.primary_lang(), "German");
        assert_eq!(cfg.secondary_lang(), "English");
        assert_eq!(cfg.ui_tab_order()[0], ProcessMode::Summarize);
        assert_eq!(cfg.hotkey_double_tap_timeout_ms(), Some(300));
        assert!(cfg.ui_single_tap_pinned());
        assert!(!cfg.ui_double_tap_pinned());
        assert_eq!(cfg.ui_default_model(), Some("groq"));
        assert_eq!(cfg.mode_default_thinking(ProcessMode::Translate), Some(ThinkingMode::Think));
        assert_eq!(cfg.mode_default_thinking(ProcessMode::Explain), Some(ThinkingMode::NoThink));
        assert_eq!(cfg.mode_default_thinking(ProcessMode::Summarize), None, "override removed");
        assert!(!out.contains("[rephrase]"), "no empty section for an untouched default: {out}");
    }

    #[test]
    fn apply_patch_without_default_model_keeps_existing_key() {
        let mut doc: toml_edit::Document = "[ui]\ndefault_model = \"keep\"\n".parse().unwrap();
        let mut patch = sample_patch();
        patch.default_model = None;
        apply_patch(&mut doc, &patch);
        let cfg: Config = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(cfg.ui_default_model(), Some("keep"));
    }

    #[test]
    fn save_to_rewrites_the_file_atomically() {
        let dir = std::env::temp_dir().join(format!("clip-llm-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, DOC).unwrap();
        let written = save_to(&path, &sample_patch()).unwrap();
        assert_eq!(written, path);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("primary   = \"German\""));
        assert!(text.contains("# top comment stays"));
        // No stray temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != "config.toml")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        let missing = dir.join("nope").join("config.toml");
        assert!(save_to(&missing, &sample_patch()).is_err());
        let bad = dir.join("bad.toml");
        std::fs::write(&bad, "[languages\nx").unwrap();
        assert!(save_to(&bad, &sample_patch()).unwrap_err().contains("TOML"));
    }

    #[test]
    fn mode_keys_match_the_config_parser() {
        for &mode in ProcessMode::ALL {
            let toml = format!("[{}]\nthinking = \"think\"\n", mode_key(mode));
            let cfg: Config = toml::from_str(&toml).unwrap();
            assert_eq!(cfg.mode_default_thinking(mode), Some(ThinkingMode::Think), "{mode:?}");
        }
        let _ = all_default_thinking();
    }
}
