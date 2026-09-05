//! In-app settings panel model: the live config read into an editable form,
//! validated into a [`SettingsPatch`], and written back into `config.toml` in
//! place (comments and unrelated keys untouched — see ADR-0001).

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::{ProcessMode, ThinkingMode};

/// Accepted range for `[hotkey].double_tap_timeout_ms`.
pub const DOUBLE_TAP_MS_RANGE: std::ops::RangeInclusive<u64> = 100..=2000;

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
    /// Selectable model profiles (pool order) and the one to start with.
    pub model_names: Vec<String>,
    pub default_model: usize,
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
}

impl SettingsForm {
    /// Snapshot of the live config for editing.
    pub fn from_config(config: &Config, model_names: Vec<String>, active_model: usize) -> Self {
        let tabs = config.ui_tab_order();
        let default_model = active_model.min(model_names.len().saturating_sub(1));
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
            model_names,
            default_model,
            error: None,
            notice: None,
        }
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
        let default_model = if self.model_names.len() > 1 {
            self.model_names.get(self.default_model).cloned()
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
        let form = SettingsForm::from_config(&cfg, vec!["a".into(), "b".into()], 1);
        assert_eq!(form.primary, "Japanese");
        assert_eq!(form.secondary, "English");
        assert_eq!(form.double_tap_ms, "400");
        assert!(form.single_tap_pinned);
        assert!(!form.double_tap_pinned);
        assert_eq!(form.default_model, 1);
        assert_eq!(form.thinking.len(), ProcessMode::ALL.len());
        assert!(form
            .thinking
            .contains(&(ProcessMode::Summarize, Some(ThinkingMode::NoThink))));
        assert!(form.thinking.contains(&(ProcessMode::Translate, None)));
        assert_eq!(form.error, None);
    }

    #[test]
    fn from_config_default_double_tap_is_the_hotkey_default() {
        let form = SettingsForm::from_config(&Config::default(), vec![], 0);
        assert_eq!(form.double_tap_ms, crate::hotkey::DEFAULT_DOUBLE_TAP_TIMEOUT.as_millis().to_string());
        assert_eq!(form.primary, "Korean");
    }

    #[test]
    fn to_patch_puts_default_mode_first_and_keeps_the_rest_in_order() {
        let mut form = SettingsForm::from_config(&Config::default(), vec!["a".into(), "b".into()], 0);
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
        assert_eq!(patch.default_model.as_deref(), Some("b"));
        assert_eq!(u128::from(patch.double_tap_ms), crate::hotkey::DEFAULT_DOUBLE_TAP_TIMEOUT.as_millis());
    }

    #[test]
    fn to_patch_single_profile_leaves_default_model_alone() {
        let form = SettingsForm::from_config(&Config::default(), vec!["only".into()], 0);
        assert_eq!(form.to_patch().unwrap().default_model, None);
    }

    #[test]
    fn to_patch_validates_languages_and_timeout() {
        let mut form = SettingsForm::from_config(&Config::default(), vec![], 0);
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
