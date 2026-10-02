//! The ACP `model` session config option.
//!
//! ACP clients pick a session's model through a `select` config option: the
//! agent advertises it in `session/new`, the client answers with
//! `session/set_config_option`, and the agent replies with the full option
//! set. This module holds the pure half: the pickable models, the option
//! value sent on the wire, and reading a model id out of a set request.
//! Applying the switch lives with the session state in the parent module.

use std::collections::HashMap;
use std::fmt;

use agent_client_protocol::schema::v1::{
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    SetSessionConfigOptionRequest,
};

use crate::config::ProviderEntry;

/// The config option id dirge uses for model selection.
pub const MODEL_CONFIG_ID: &str = "model";

/// A model the client may pick, with the provider aliases that pin it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub model: String,
    pub aliases: Vec<String>,
}

impl ModelChoice {
    fn into_select_option(self) -> SessionConfigSelectOption {
        let description = (!self.aliases.is_empty()).then(|| self.aliases.join(", "));
        SessionConfigSelectOption::new(self.model.clone(), self.model).description(description)
    }
}

/// `(model, alias, is_active)` rows for the configured providers that pin a
/// model, sorted. The `/model` listing prints these.
pub fn configured_models(
    providers: &HashMap<String, ProviderEntry>,
    current: &str,
) -> Vec<(String, String, bool)> {
    let mut rows: Vec<(String, String, bool)> = providers
        .iter()
        .filter_map(|(alias, entry)| {
            entry
                .model
                .as_ref()
                .map(|m| (m.clone(), alias.clone(), m == current))
        })
        .collect();
    rows.sort();
    rows
}

/// The pickable models: every model a provider pins, one entry per model id
/// (aliases that pin the same model are merged), with the current model
/// first when no provider pins it.
pub fn model_choices(
    providers: &HashMap<String, ProviderEntry>,
    current: &str,
) -> Vec<ModelChoice> {
    let mut choices: Vec<ModelChoice> = Vec::new();
    for (model, alias, _) in configured_models(providers, current) {
        match choices.iter_mut().find(|c| c.model == model) {
            Some(choice) => choice.aliases.push(alias),
            None => choices.push(ModelChoice {
                model,
                aliases: vec![alias],
            }),
        }
    }
    if !current.is_empty() && !choices.iter().any(|c| c.model == current) {
        choices.insert(
            0,
            ModelChoice {
                model: current.to_string(),
                aliases: Vec::new(),
            },
        );
    }
    choices
}

/// The `model` select option with `current` selected.
pub fn model_config_option(
    providers: &HashMap<String, ProviderEntry>,
    current: &str,
) -> SessionConfigOption {
    let options: Vec<SessionConfigSelectOption> = model_choices(providers, current)
        .into_iter()
        .map(ModelChoice::into_select_option)
        .collect();
    SessionConfigOption::select(MODEL_CONFIG_ID, "Model", current.to_string(), options)
        .category(SessionConfigOptionCategory::Model)
        .description("Model for the next prompt in this session".to_string())
}

/// Why a `session/set_config_option` request names no model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelRequestError {
    /// The request targets an option dirge does not offer.
    UnknownOption(String),
    /// The value is not a value id (a boolean, for example).
    NotAValueId,
    /// The value id is blank.
    EmptyModel,
}

impl fmt::Display for ModelRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption(id) => write!(f, "unknown config option '{id}'"),
            Self::NotAValueId => write!(f, "config option '{MODEL_CONFIG_ID}' takes a model id"),
            Self::EmptyModel => write!(
                f,
                "config option '{MODEL_CONFIG_ID}' needs a non-empty model id"
            ),
        }
    }
}

/// The model id a set request asks for.
pub fn requested_model(req: &SetSessionConfigOptionRequest) -> Result<&str, ModelRequestError> {
    if &*req.config_id.0 != MODEL_CONFIG_ID {
        return Err(ModelRequestError::UnknownOption(req.config_id.to_string()));
    }
    let value = req
        .value
        .as_value_id()
        .ok_or(ModelRequestError::NotAValueId)?;
    let model = value.0.trim();
    if model.is_empty() {
        return Err(ModelRequestError::EmptyModel);
    }
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn providers(rows: &[(&str, Option<&str>)]) -> HashMap<String, ProviderEntry> {
        rows.iter()
            .map(|(alias, model)| {
                let entry = ProviderEntry {
                    model: model.map(str::to_string),
                    ..Default::default()
                };
                (alias.to_string(), entry)
            })
            .collect()
    }

    fn set_request(json: serde_json::Value) -> SetSessionConfigOptionRequest {
        serde_json::from_value(json).expect("a valid set_config_option request")
    }

    #[test]
    fn choices_merge_aliases_and_skip_unpinned_providers() {
        let p = providers(&[
            ("a", Some("m1")),
            ("b", Some("m1")),
            ("c", Some("m2")),
            ("d", None),
        ]);
        let choices = model_choices(&p, "m2");
        let models: Vec<&str> = choices.iter().map(|c| c.model.as_str()).collect();
        assert_eq!(models, vec!["m1", "m2"]);
        assert_eq!(choices[0].aliases, vec!["a", "b"]);
        assert_eq!(choices[1].aliases, vec!["c"]);
    }

    #[test]
    fn choices_put_an_unpinned_current_model_first() {
        let p = providers(&[("a", Some("m1"))]);
        let choices = model_choices(&p, "other");
        assert_eq!(choices[0].model, "other");
        assert!(choices[0].aliases.is_empty());
        assert_eq!(choices.len(), 2);
    }

    #[test]
    fn choices_without_providers_offer_the_current_model() {
        let choices = model_choices(&HashMap::new(), "solo");
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].model, "solo");
        assert!(model_choices(&HashMap::new(), "").is_empty());
    }

    #[test]
    fn option_serializes_as_a_model_select() {
        let p = providers(&[("fast", Some("m1"))]);
        let json = serde_json::to_value(model_config_option(&p, "m1")).unwrap();
        assert_eq!(json["id"], "model");
        assert_eq!(json["type"], "select");
        assert_eq!(json["category"], "model");
        assert_eq!(json["currentValue"], "m1");
        assert_eq!(json["options"][0]["value"], "m1");
        assert_eq!(json["options"][0]["description"], "fast");
    }

    #[test]
    fn requested_model_reads_a_value_id() {
        let req = set_request(serde_json::json!({
            "sessionId": "s", "configId": "model", "value": " gpt-x "
        }));
        assert_eq!(requested_model(&req), Ok("gpt-x"));
    }

    #[test]
    fn requested_model_rejects_other_options_and_shapes() {
        let other = set_request(serde_json::json!({
            "sessionId": "s", "configId": "effort", "value": "high"
        }));
        assert_eq!(
            requested_model(&other),
            Err(ModelRequestError::UnknownOption("effort".to_string()))
        );
        let boolean = set_request(serde_json::json!({
            "sessionId": "s", "configId": "model", "type": "boolean", "value": true
        }));
        assert_eq!(
            requested_model(&boolean),
            Err(ModelRequestError::NotAValueId)
        );
        let blank = set_request(serde_json::json!({
            "sessionId": "s", "configId": "model", "value": "  "
        }));
        assert_eq!(requested_model(&blank), Err(ModelRequestError::EmptyModel));
    }
}
