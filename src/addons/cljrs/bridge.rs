//! Data crossing the cljrs boundary: JSON in, JSON out.
//!
//! Only plain data crosses. Object keys become keywords on the way in, so an
//! IAddon handler reads `(:rows params)` exactly as it does on the JVM; on
//! the way out the Clojure host has already folded values through its
//! `json-safe`, so maps, vectors, strings, numbers, keywords,
//! booleans and nil are all this has to read. Anything else is printed.

use cljrs_gc::GcPtr;
use cljrs_value::{Keyword, MapValue, PersistentVector, Value};
use serde_json::{Map, Number, Value as Json};

/// JSON as a Clojure value.
pub fn to_clj(json: &Json) -> Value {
    match json {
        Json::Null => Value::Nil,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Value::Long(i),
            None => Value::Double(n.as_f64().unwrap_or(f64::NAN)),
        },
        Json::String(s) => Value::Str(GcPtr::new(s.clone())),
        Json::Array(items) => Value::Vector(GcPtr::new(PersistentVector::from_iter(
            items.iter().map(to_clj),
        ))),
        Json::Object(entries) => Value::Map(MapValue::from_pairs(
            entries
                .iter()
                .map(|(k, v)| (Value::keyword(Keyword::parse(k)), to_clj(v)))
                .collect(),
        )),
    }
}

/// A Clojure value as JSON.
pub fn to_json(value: &Value) -> Json {
    match value {
        Value::Nil => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Long(i) => Json::from(*i),
        Value::Double(d) => Number::from_f64(*d).map_or(Json::Null, Json::Number),
        Value::Str(s) => Json::String(s.get().clone()),
        Value::Keyword(k) => Json::String(k.get().full_name()),
        Value::Vector(v) => Json::Array(v.get().iter().map(to_json).collect()),
        Value::List(l) => Json::Array(l.get().iter().map(to_json).collect()),
        Value::Map(m) => {
            let mut out = Map::new();
            m.for_each(|k, v| {
                out.insert(key_string(k), to_json(v));
            });
            Json::Object(out)
        }
        Value::WithMeta(inner, _) => to_json(inner),
        other => Json::String(other.to_string()),
    }
}

fn key_string(key: &Value) -> String {
    match key {
        Value::Str(s) => s.get().clone(),
        Value::Keyword(k) => k.get().full_name(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_data_round_trips() {
        let data = json!({
            "rows": [1, 2.5, "x", null, true, {"nested": []}],
            "ns/qualified": "v"
        });
        assert_eq!(to_json(&to_clj(&data)), data);
    }

    #[test]
    fn object_keys_arrive_as_keywords() {
        let Value::Map(m) = to_clj(&json!({"rows": 3})) else {
            panic!("expected a map");
        };
        let got = m.get(&Value::keyword(Keyword::simple("rows")));
        assert!(matches!(got, Some(Value::Long(3))), "{got:?}");
    }

    #[test]
    fn keywords_leave_without_their_colon() {
        let v = Value::keyword(Keyword::qualified("dirge", "on-prompt"));
        assert_eq!(to_json(&v), json!("dirge/on-prompt"));
    }

    #[test]
    fn non_finite_doubles_become_null() {
        assert_eq!(to_json(&Value::Double(f64::NAN)), Json::Null);
    }
}
