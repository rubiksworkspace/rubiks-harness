//! The model catalog as the protocol sees it, and how a request names a model.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

/// One model Prime Agent knows about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    pub id: String,
    pub provider: String,
    pub label: String,
    /// Whether a task naming this model could run now (its provider has auth).
    pub available: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ModelCatalog {
    /// The harness's default model, as a selector [`ModelCatalog::resolve`] accepts.
    pub default: Option<String>,
    /// Provider whose entry wins when several providers carry the same model id.
    pub preferred_provider: Option<String>,
    pub entries: Vec<ModelEntry>,
}

impl ModelCatalog {
    /// The available entry a selector names: a bare id (the preferred provider's
    /// entry first) or `provider/id`. Ids may contain `/` themselves, so the
    /// whole selector is tried as an id before being split.
    #[must_use]
    pub fn resolve(&self, selector: &str) -> Option<&ModelEntry> {
        let available = || self.entries.iter().filter(|entry| entry.available);
        let preferred = self.preferred_provider.as_deref();
        available()
            .find(|entry| entry.id == selector && preferred == Some(entry.provider.as_str()))
            .or_else(|| available().find(|entry| entry.id == selector))
            .or_else(|| {
                let (provider, id) = selector.split_once('/')?;
                available().find(|entry| entry.provider == provider && entry.id == id)
            })
    }

    /// The model a task runs when it names none.
    #[must_use]
    pub fn default_entry(&self) -> Option<&ModelEntry> {
        self.default
            .as_deref()
            .and_then(|model| self.resolve(model))
    }

    fn id_counts(&self) -> HashMap<&str, usize> {
        let mut counts = HashMap::new();
        for entry in &self.entries {
            *counts.entry(entry.id.as_str()).or_insert(0) += 1;
        }
        counts
    }

    /// `GET /v1/models`: the catalog grouped by provider.
    #[must_use]
    pub fn backends_json(&self) -> Value {
        let default = self.default_entry();
        let mut seen = HashSet::new();
        let mut backends = Map::new();
        for provider in self.entries.iter().map(|entry| entry.provider.as_str()) {
            if !seen.insert(provider) {
                continue;
            }
            let entries: Vec<&ModelEntry> = self
                .entries
                .iter()
                .filter(|entry| entry.provider == provider)
                .collect();
            let is_default = |entry: &ModelEntry| default == Some(entry);
            let backend_default = entries
                .iter()
                .find(|entry| is_default(entry))
                .or_else(|| entries.iter().find(|entry| entry.available))
                .or_else(|| entries.first())
                .map_or("", |entry| entry.id.as_str());
            let models: Vec<Value> = entries
                .iter()
                .map(|entry| {
                    json!({
                        "id": entry.id,
                        "label": entry.label,
                        "backend": entry.provider,
                        "available": entry.available,
                        "default": entry.id == backend_default,
                    })
                })
                .collect();
            backends.insert(
                provider.to_string(),
                json!({ "default": backend_default, "models": models }),
            );
        }
        json!({ "backends": backends })
    }

    /// The `models` array of `GET /v1/harnesses/{id}/models`.
    #[must_use]
    pub fn harness_models_json(&self) -> Vec<Value> {
        let default = self.default_entry();
        let counts = self.id_counts();
        self.entries
            .iter()
            .map(|entry| {
                json!({
                    "id": selector(entry, &counts),
                    "label": entry.label,
                    "available": entry.available,
                    "default": default == Some(entry),
                })
            })
            .collect()
    }

    /// What the catalog calls the default model, if one can run.
    #[must_use]
    pub fn default_selector(&self) -> Option<String> {
        let counts = self.id_counts();
        self.default_entry().map(|entry| selector(entry, &counts))
    }
}

/// The id a client should send for an entry: bare unless another provider
/// carries the same id, when only the `provider/id` form is unambiguous.
fn selector(entry: &ModelEntry, counts: &HashMap<&str, usize>) -> String {
    if counts.get(entry.id.as_str()).copied().unwrap_or(0) > 1 {
        format!("{}/{}", entry.provider, entry.id)
    } else {
        entry.id.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(provider: &str, id: &str, available: bool) -> ModelEntry {
        ModelEntry {
            id: id.to_string(),
            provider: provider.to_string(),
            label: id.to_string(),
            available,
        }
    }

    fn catalog() -> ModelCatalog {
        ModelCatalog {
            default: Some("b/shared".to_string()),
            preferred_provider: Some("b".to_string()),
            entries: vec![
                entry("a", "shared", true),
                entry("b", "shared", true),
                entry("a", "vendor/slashed", true),
                entry("a", "locked", false),
            ],
        }
    }

    #[test]
    fn a_bare_id_prefers_the_preferred_provider() {
        assert_eq!(catalog().resolve("shared").unwrap().provider, "b");
    }

    #[test]
    fn a_provider_selector_and_a_slashed_id_both_resolve() {
        assert_eq!(catalog().resolve("a/shared").unwrap().provider, "a");
        assert_eq!(catalog().resolve("vendor/slashed").unwrap().provider, "a");
    }

    #[test]
    fn a_model_without_auth_or_unknown_does_not_resolve() {
        assert!(catalog().resolve("locked").is_none());
        assert!(catalog().resolve("nope").is_none());
    }

    #[test]
    fn duplicate_ids_are_listed_with_their_provider() {
        let models = catalog().harness_models_json();
        assert_eq!(models[0]["id"], "a/shared");
        assert_eq!(models[1]["id"], "b/shared");
        assert_eq!(models[1]["default"], true);
        assert_eq!(models[3]["id"], "locked");
        assert_eq!(models[3]["available"], false);
    }

    #[test]
    fn every_backend_names_a_default_it_lists() {
        let backends = catalog().backends_json();
        assert_eq!(backends["backends"]["b"]["default"], "shared");
        assert_eq!(backends["backends"]["a"]["default"], "shared");
        assert_eq!(backends["backends"]["a"]["models"][0]["default"], true);
    }
}
