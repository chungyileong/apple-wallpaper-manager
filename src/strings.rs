use std::{collections::HashMap, path::Path, process::Command};

use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, Default)]
pub struct StringsCatalog {
    labels: HashMap<String, String>,
}

impl StringsCatalog {
    pub fn lookup(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(|value| value.as_str())
    }

    #[cfg(test)]
    pub(crate) fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        let mut labels = HashMap::new();
        for (key, value) in pairs {
            labels.insert((*key).to_owned(), (*value).to_owned());
        }
        Self { labels }
    }
}

#[derive(Debug, Error)]
pub enum StringsError {
    #[error("failed to read strings bundle: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to convert strings bundle to json: {0}")]
    Convert(String),
    #[error("failed to parse strings json: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn load_strings(path: &Path) -> Result<StringsCatalog, StringsError> {
    let json = convert_strings_to_json(path)?;
    let value: Value = serde_json::from_str(&json)?;
    Ok(StringsCatalog {
        labels: extract_labels(value, path),
    })
}

fn convert_strings_to_json(path: &Path) -> Result<String, StringsError> {
    let output = Command::new("plutil")
        .args([
            "-convert",
            "json",
            "-o",
            "-",
            path.to_string_lossy().as_ref(),
        ])
        .output()?;

    if !output.status.success() {
        return Err(StringsError::Convert(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }

    String::from_utf8(output.stdout).map_err(|err| StringsError::Convert(err.to_string()))
}

fn extract_labels(value: Value, path: &Path) -> HashMap<String, String> {
    if is_loctable(path)
        && let Some(labels) = select_loctable_locale(&value)
    {
        return labels;
    }

    flatten_string_dict(value)
}

fn is_loctable(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("loctable"))
        .unwrap_or(false)
}

fn select_loctable_locale(value: &Value) -> Option<HashMap<String, String>> {
    let map = value.as_object()?;
    for locale in locale_preferences() {
        if let Some(entry) = map.get(&locale) {
            let labels = flatten_string_dict(entry.clone());
            if !labels.is_empty() {
                return Some(labels);
            }
        }
    }

    for entry in map.values() {
        let labels = flatten_string_dict(entry.clone());
        if !labels.is_empty() {
            return Some(labels);
        }
    }

    None
}

fn flatten_string_dict(value: Value) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    if let Some(map) = value.as_object() {
        for (key, value) in map {
            if let Some(text) = value.as_str() {
                labels.insert(key.clone(), text.to_owned());
            }
        }
    }
    labels
}

fn locale_preferences() -> Vec<String> {
    let mut candidates = Vec::new();
    for env_name in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(env_name)
            && let Some(locale) = normalize_locale(&value)
        {
            candidates.push(locale.clone());
            if let Some(short) = locale.split('_').next()
                && !short.is_empty()
            {
                candidates.push(short.to_owned());
            }
        }
    }

    candidates.push("en".to_owned());

    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn strings_catalog_lookup_finds_existing_key() {
        let catalog = StringsCatalog::from_pairs(&[("Greeting", "Hello"), ("Farewell", "Goodbye")]);
        assert_eq!(catalog.lookup("Greeting"), Some("Hello"));
        assert_eq!(catalog.lookup("Farewell"), Some("Goodbye"));
        assert_eq!(catalog.lookup("Missing"), None);
    }

    #[test]
    fn normalize_locale_strips_encoding_suffix() {
        assert_eq!(normalize_locale("en_US.UTF-8"), Some("en_US".to_owned()));
    }

    #[test]
    fn normalize_locale_strips_modifier_suffix() {
        assert_eq!(normalize_locale("de_DE@euro"), Some("de_DE".to_owned()));
    }

    #[test]
    fn normalize_locale_normalizes_dashes_to_underscores() {
        assert_eq!(normalize_locale("en-US"), Some("en_US".to_owned()));
    }

    #[test]
    fn normalize_locale_returns_none_for_empty_or_whitespace() {
        assert_eq!(normalize_locale(""), None);
        assert_eq!(normalize_locale("   "), None);
    }

    #[test]
    fn is_loctable_detects_loctable_extension() {
        assert!(is_loctable(Path::new("bundle.loctable")));
        assert!(is_loctable(Path::new("bundle.LOCTABLE")));
        assert!(!is_loctable(Path::new("bundle.strings")));
        assert!(!is_loctable(Path::new("bundle")));
    }

    #[test]
    fn flatten_string_dict_extracts_string_values() {
        let json = serde_json::json!({"Hello": "World", "Foo": "Bar"});
        let result = flatten_string_dict(json);
        assert_eq!(result.get("Hello").map(|s| s.as_str()), Some("World"));
        assert_eq!(result.get("Foo").map(|s| s.as_str()), Some("Bar"));
    }

    #[test]
    fn flatten_string_dict_skips_non_string_values() {
        let json = serde_json::json!({"num": 42, "name": "value"});
        let result = flatten_string_dict(json);
        assert!(!result.contains_key("num"));
        assert_eq!(result.get("name").map(|s| s.as_str()), Some("value"));
    }

    #[test]
    fn select_loctable_locale_picks_english_when_present() {
        // "en" is always appended to locale_preferences() as the final fallback
        let json = serde_json::json!({
            "en": {"Greeting": "Hello", "Farewell": "Goodbye"}
        });
        let result = select_loctable_locale(&json).unwrap();
        assert_eq!(result.get("Greeting").map(|s| s.as_str()), Some("Hello"));
    }

    #[test]
    fn select_loctable_locale_falls_back_to_first_available_locale() {
        let json = serde_json::json!({
            "xx-nosuchthing": {"Key": "Value"}
        });
        // No locale preference matches "xx-nosuchthing", so the fallback loop
        // over all map values should still return a result.
        let result = select_loctable_locale(&json);
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().get("Key").map(|s| s.as_str()),
            Some("Value")
        );
    }
}

fn normalize_locale(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    let without_encoding = trimmed.split('.').next().unwrap_or(trimmed);
    let without_modifier = without_encoding
        .split('@')
        .next()
        .unwrap_or(without_encoding);
    let normalized = without_modifier.replace('-', "_");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}
