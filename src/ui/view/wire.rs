//! The JSON codec between the view domain and an engine that speaks
//! plain data (the cljrs isolate). Translation only: no policy.

use serde_json::Value as Json;

use super::domain::{NoticeLevel, ViewEffect, ViewEvent, ViewModel, ViewUpdate};

/// `event` as the engine reads it: `{"type": "command", ...}`.
pub fn encode_event(event: &ViewEvent) -> Json {
    serde_json::to_value(event).unwrap_or(Json::Null)
}

/// Read an engine answer `{"model": .., "effects": [..]}`. A bad model
/// fails the update; an effect the UI does not understand becomes an
/// error notice instead of failing it.
pub fn decode_update(answer: &Json) -> Result<ViewUpdate, String> {
    let model: ViewModel =
        serde_json::from_value(answer.get("model").cloned().unwrap_or(Json::Null))
            .map_err(|e| format!("bad view model: {e}"))?;
    let effects = answer
        .get("effects")
        .and_then(Json::as_array)
        .map(|items| items.iter().map(decode_effect).collect())
        .unwrap_or_default();
    Ok(ViewUpdate { model, effects })
}

fn decode_effect(item: &Json) -> ViewEffect {
    serde_json::from_value(item.clone()).unwrap_or_else(|e| {
        ViewEffect::notify(
            NoticeLevel::Error,
            format!("view effect ignored ({e}): {item}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::view::domain::{GridCell, SwarmModel};
    use serde_json::json;

    #[test]
    fn events_encode_with_a_type_tag() {
        assert_eq!(encode_event(&ViewEvent::Init), json!({"type": "init"}));
        assert_eq!(
            encode_event(&ViewEvent::command("swarm", &["on"])),
            json!({"type": "command", "name": "swarm", "args": ["on"]})
        );
        assert_eq!(
            encode_event(&ViewEvent::Grid {
                key: "Tab".into(),
                cells: vec![GridCell::Panel("a".into()), GridCell::Agent("t1".into())],
                columns: 2
            }),
            json!({"type": "grid", "key": "Tab", "columns": 2, "cells": [
                {"kind": "panel", "id": "a"},
                {"kind": "agent", "id": "t1"}
            ]})
        );
    }

    #[test]
    fn updates_decode_and_bad_effects_become_notices() {
        let update = decode_update(&json!({
            "model": {
                "swarm": {"selected": {"kind": "agent", "id": "t1"}},
                "grid_keys": ["Esc"],
                "view_commands": ["swarm"]
            },
            "effects": [
                {"op": "reply", "action": "focus", "target": "b"},
                {"op": "panel-status"},
                {"op": "open-agent", "id": "t1"},
                {"op": "teleport"}
            ]
        }))
        .unwrap();
        assert_eq!(
            update.model.swarm,
            Some(SwarmModel {
                selected: Some(GridCell::Agent("t1".into()))
            })
        );
        assert_eq!(
            update.effects[0],
            ViewEffect::Reply {
                action: "focus".into(),
                target: Some("b".into())
            }
        );
        assert_eq!(update.effects[1], ViewEffect::PanelStatus);
        assert_eq!(update.effects[2], ViewEffect::OpenAgent { id: "t1".into() });
        assert!(matches!(
            &update.effects[3],
            ViewEffect::Notify { level: NoticeLevel::Error, text } if text.contains("teleport")
        ));
    }

    #[test]
    fn a_bad_model_fails_the_update() {
        assert!(decode_update(&json!({"model": {"swarm": 3}})).is_err());
    }
}
