//! Reviewer settings: the repo's settings (`PATCH /repos/{o}/{r}/reviewer`, `roll code reviewer`)
//! overlaid with the `reviewer:` section of `.roll/review.yaml` at the default branch's head.
//! The file wins for every key it sets: it's code, reviewed like the rest.

use crate::mode::{Mode, ModeChoice, Speed};
use crate::provider::Effort;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Speaks up about bugs and security; approves the rest.
    Chill,
    /// Comments on everything it finds and asks for changes more readily.
    Assertive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Classifier {
    Heuristic,
    /// One small model call scores the change first.
    Model,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutoSettings {
    pub deep_paths: Vec<String>,
    pub max_paths: Vec<String>,
    /// Changes smaller than this (lines added + removed) with nothing sensitive get `standard`.
    pub standard_below_lines: u64,
    pub classifier: Classifier,
}

impl Default for AutoSettings {
    fn default() -> Self {
        AutoSettings { deep_paths: Vec::new(), max_paths: Vec::new(), standard_below_lines: 80, classifier: Classifier::Heuristic }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathInstruction {
    pub path: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewerSettings {
    pub enabled: bool,
    pub mode: ModeChoice,
    pub auto: AutoSettings,
    pub speed: Speed,
    /// Review on open and on every revision (incrementally). Off: only `@roll review`.
    pub auto_review: bool,
    pub drafts: bool,
    /// Counts toward the repo's rules as a required review (`review_pending` until done).
    pub required: bool,
    pub profile: Profile,
    pub ignore: Vec<String>,
    pub instructions: Vec<PathInstruction>,
    pub rules: Vec<String>,
    /// Appended to roll's system prompt (inside the cached prefix).
    pub prompt: Option<String>,
    /// A file at the default branch's head whose content is appended like `prompt`.
    pub prompt_file: Option<String>,
    /// Overrides the provider's model.
    pub model: Option<String>,
    /// The cheaper model for `fast` and the classifier.
    pub secondary_model: Option<String>,
    /// Overrides the effort the mode and speed pick.
    pub effort: Option<Effort>,
    /// A mermaid diagram when control flow changed.
    pub diagram: bool,
    /// Changes touching more files than this are reviewed in part (the rest is listed).
    pub max_files: u32,
}

impl Default for ReviewerSettings {
    fn default() -> Self {
        ReviewerSettings {
            enabled: false,
            mode: ModeChoice::Auto,
            auto: AutoSettings::default(),
            speed: Speed::Balanced,
            auto_review: true,
            drafts: false,
            required: false,
            profile: Profile::Chill,
            ignore: Vec::new(),
            instructions: Vec::new(),
            rules: Vec::new(),
            prompt: None,
            prompt_file: None,
            model: None,
            secondary_model: None,
            effort: None,
            diagram: true,
            max_files: 300,
        }
    }
}

const MAX_PROMPT: usize = 16 << 10;
const MAX_LIST: usize = 100;

impl ReviewerSettings {
    /// Overlay `v` (a `reviewer:` section or a PATCH body) on these settings. Unknown keys and
    /// bad values are reported, not applied; everything else is.
    pub fn overlay(&mut self, v: &Value) -> Vec<String> {
        let mut errs = Vec::new();
        let Some(o) = v.as_object() else {
            if !v.is_null() {
                errs.push("reviewer settings must be a mapping".into());
            }
            return errs;
        };
        for (k, val) in o {
            if let Err(e) = self.set(k, val) {
                errs.push(format!("reviewer.{k}: {e}"));
            }
        }
        errs
    }

    fn set(&mut self, k: &str, v: &Value) -> Result<(), String> {
        match k {
            "enabled" => self.enabled = boolean(v)?,
            "mode" => self.mode = ModeChoice::parse(string(v)?).ok_or("auto, standard, deep or max")?,
            "speed" => self.speed = Speed::parse(string(v)?).ok_or("fast, balanced or thorough")?,
            "auto_review" => self.auto_review = boolean(v)?,
            "drafts" => self.drafts = boolean(v)?,
            "required" => self.required = boolean(v)?,
            "diagram" => self.diagram = boolean(v)?,
            "profile" => {
                self.profile = match string(v)? {
                    "chill" => Profile::Chill,
                    "assertive" => Profile::Assertive,
                    _ => return Err("chill or assertive".into()),
                }
            }
            "ignore" => self.ignore = strings(v)?,
            "rules" => self.rules = strings(v)?,
            "instructions" => {
                let a = v.as_array().ok_or("a list of {path, text}")?;
                if a.len() > MAX_LIST {
                    return Err(format!("at most {MAX_LIST}"));
                }
                let mut out = Vec::new();
                for i in a {
                    let path = i.get("path").and_then(Value::as_str).ok_or("each needs a path")?;
                    let text = i.get("text").and_then(Value::as_str).ok_or("each needs a text")?;
                    if text.len() > MAX_PROMPT {
                        return Err("an instruction is too long".into());
                    }
                    out.push(PathInstruction { path: path.into(), text: text.into() });
                }
                self.instructions = out;
            }
            "prompt" => self.prompt = opt_string(v, MAX_PROMPT)?,
            "prompt_file" => self.prompt_file = opt_string(v, 1024)?,
            "model" => self.model = opt_string(v, 200)?,
            "secondary_model" => self.secondary_model = opt_string(v, 200)?,
            "effort" => {
                self.effort = match v {
                    Value::Null => None,
                    v => Some(Effort::parse(string(v)?).ok_or("low, medium, high, xhigh or max")?),
                }
            }
            "max_files" => {
                let n = v.as_u64().ok_or("a number")?;
                if !(1..=5000).contains(&n) {
                    return Err("between 1 and 5000".into());
                }
                self.max_files = n as u32;
            }
            "auto" => {
                let o = v.as_object().ok_or("a mapping")?;
                for (k, v) in o {
                    match k.as_str() {
                        "deep_paths" => self.auto.deep_paths = strings(v)?,
                        "max_paths" => self.auto.max_paths = strings(v)?,
                        "standard_below_lines" => self.auto.standard_below_lines = v.as_u64().ok_or("auto.standard_below_lines: a number")?,
                        "classifier" => {
                            self.auto.classifier = match string(v)? {
                                "heuristic" => Classifier::Heuristic,
                                "model" => Classifier::Model,
                                _ => return Err("auto.classifier: heuristic or model".into()),
                            }
                        }
                        other => return Err(format!("auto.{other} isn't a setting")),
                    }
                }
            }
            other => return Err(format!("{other} isn't a setting")),
        }
        Ok(())
    }

    /// The settings as a JSON object (what the API shows and stores).
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Object(Map::new()))
    }

    /// The fixed mode, or `None` for `auto`.
    pub fn fixed_mode(&self) -> Option<Mode> {
        match self.mode {
            ModeChoice::Auto => None,
            ModeChoice::Fixed(m) => Some(m),
        }
    }
}

fn boolean(v: &Value) -> Result<bool, String> {
    v.as_bool().ok_or_else(|| "true or false".into())
}

fn string(v: &Value) -> Result<&str, String> {
    v.as_str().ok_or_else(|| "a string".into())
}

fn opt_string(v: &Value, max: usize) -> Result<Option<String>, String> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) if s.len() <= max => Ok(Some(s.clone()).filter(|s| !s.trim().is_empty())),
        Value::String(_) => Err(format!("at most {max} bytes")),
        _ => Err("a string".into()),
    }
}

fn strings(v: &Value) -> Result<Vec<String>, String> {
    let a = v.as_array().ok_or("a list of strings")?;
    if a.len() > MAX_LIST {
        return Err(format!("at most {MAX_LIST}"));
    }
    a.iter().map(|s| s.as_str().map(str::to_string).ok_or_else(|| "a list of strings".to_string())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn overlay_and_errors() {
        let mut s = ReviewerSettings::default();
        assert!(!s.enabled);
        let errs = s.overlay(&json!({
            "enabled": true, "mode": "deep", "speed": "fast", "profile": "assertive",
            "ignore": ["*.lock"], "instructions": [{"path": "src/**", "text": "be careful"}],
            "rules": ["no unwrap"], "prompt": "Be terse.", "effort": "high", "max_files": 50,
            "auto": {"deep_paths": ["auth/**"], "classifier": "model", "standard_below_lines": 40},
            "bogus": 1, "drafts": "yes"
        }));
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("bogus")));
        assert!(s.enabled && s.speed == Speed::Fast && s.profile == Profile::Assertive);
        assert_eq!(s.fixed_mode(), Some(Mode::Deep));
        assert_eq!(s.auto.classifier, Classifier::Model);
        assert_eq!(s.auto.standard_below_lines, 40);
        assert_eq!(s.effort, Some(Effort::High));
        assert_eq!(s.instructions[0].text, "be careful");
        s.overlay(&json!({"mode": "auto", "effort": null, "prompt": ""}));
        assert_eq!(s.fixed_mode(), None);
        assert_eq!(s.effort, None);
        assert_eq!(s.prompt, None);
        // Round-trips through its JSON form.
        let mut t = ReviewerSettings::default();
        assert!(t.overlay(&s.to_value()).is_empty());
        assert_eq!(t, s);
        assert!(!ReviewerSettings::default().overlay(&json!("x")).is_empty());
        assert!(ReviewerSettings::default().overlay(&json!({"max_files": 0})).len() == 1);
    }
}
