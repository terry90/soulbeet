//! Validation for Navidrome smart playlist (`.nsp`) criteria JSON.
//!
//! Grounded directly in Navidrome's `model/criteria/` Go package (`criteria.go`, `json.go`,
//! `operators.go`, `fields.go`).

use serde_json::Value;

/// Maximum nesting depth of nested `all`/`any` conjunctions.
const MAX_DEPTH: usize = 16;

/// Valid top-level keys accepted by `criteria.Criteria.UnmarshalJSON` (`criteria.go:136-143`).
const TOP_LEVEL_KEYS: &[&str] = &["all", "any", "sort", "order", "limit", "offset"];

/// Valid expression operators accepted by `unmarshalExpression` (`json.go:41-72`), lowercased.
const OPERATORS: &[&str] = &[
    "is",
    "isnot",
    "gt",
    "lt",
    "contains",
    "notcontains",
    "startswith",
    "endswith",
    "intherange",
    "before",
    "after",
    "inthelast",
    "notinthelast",
    "inplaylist",
    "notinplaylist",
];

const VALID_OPERATORS_HELP: &str = "is, isNot, gt, lt, contains, notContains, startsWith, endsWith, inTheRange, before, after, inTheLast, notInTheLast, inPlaylist, notInPlaylist, all, any";

/// Validate a Navidrome smart playlist criteria JSON value.
///
/// Returns `Ok(())` when `rules` matches Navidrome's `criteria.Criteria` JSON shape, or an
/// `Err(expected)` description suitable for `invalid_value(..., "rules", &expected)`.
pub fn validate_rules(rules: &Value) -> Result<(), String> {
    let Some(obj) = rules.as_object() else {
        return Err(
            r#"a JSON object with 'all' or 'any' rules (e.g. {"all": [{"is": {"loved": true}}], "sort": "dateAdded", "order": "desc", "limit": 50})"#.into(),
        );
    };

    for key in obj.keys() {
        if !TOP_LEVEL_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "a smart playlist rules object with only top-level keys all, any, sort, order, limit, offset (unexpected key '{key}')"
            ));
        }
    }

    let has_all = obj.contains_key("all");
    let has_any = obj.contains_key("any");
    match (has_all, has_any) {
        (false, false) => {
            return Err(
                "a smart playlist rules object containing a non-empty 'all' or 'any' array of rules"
                    .into(),
            );
        }
        (true, true) => {
            return Err(
                "a smart playlist rules object containing either 'all' or 'any' at the top level, not both"
                    .into(),
            );
        }
        (true, false) => validate_conjunction("all", &obj["all"], 0)?,
        (false, true) => validate_conjunction("any", &obj["any"], 0)?,
    }

    if let Some(sort) = obj.get("sort") {
        if !sort.is_string() {
            return Err("a smart playlist rules object whose 'sort' is a string".into());
        }
    }

    if let Some(order) = obj.get("order") {
        let valid = order
            .as_str()
            .map(|s| s.trim().to_ascii_lowercase())
            .is_some_and(|s| matches!(s.as_str(), "asc" | "desc"));
        if !valid {
            return Err(
                "a smart playlist rules object whose 'order' is 'asc' or 'desc'".into(),
            );
        }
    }

    if let Some(limit) = obj.get("limit") {
        if !limit.as_u64().is_some_and(|n| n <= i64::MAX as u64) {
            return Err(
                "a smart playlist rules object whose 'limit' is a non-negative integer".into(),
            );
        }
    }

    if let Some(offset) = obj.get("offset") {
        if !offset.as_u64().is_some_and(|n| n <= i64::MAX as u64) {
            return Err(
                "a smart playlist rules object whose 'offset' is a non-negative integer".into(),
            );
        }
    }

    Ok(())
}

fn validate_conjunction(key: &str, value: &Value, depth: usize) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!(
            "a smart playlist rules object nested at most {MAX_DEPTH} levels deep"
        ));
    }

    let Some(items) = value.as_array().filter(|arr| !arr.is_empty()) else {
        return Err(format!(
            "a smart playlist rules object whose '{key}' is a non-empty array of rule objects"
        ));
    };

    for (idx, item) in items.iter().enumerate() {
        let Some(rule_obj) = item.as_object().filter(|m| !m.is_empty()) else {
            return Err(format!(
                "a smart playlist rules object where '{key}[{idx}]' is a non-empty rule object"
            ));
        };

        for (op_key, op_val) in rule_obj {
            let op_lower = op_key.to_ascii_lowercase();
            if op_lower == "all" || op_lower == "any" {
                validate_conjunction(op_key, op_val, depth + 1)?;
            } else if OPERATORS.contains(&op_lower.as_str()) {
                validate_operator(op_key, &op_lower, op_val)?;
            } else {
                return Err(format!(
                    "a smart playlist rules object using valid operators (unknown operator '{op_key}'; valid operators: {VALID_OPERATORS_HELP})"
                ));
            }
        }
    }

    Ok(())
}

fn validate_operator(op_key: &str, op_lower: &str, op_val: &Value) -> Result<(), String> {
    // Navidrome's marshalExpression (json.go:92) rejects any operator map whose length != 1.
    let Some(field_map) = op_val.as_object().filter(|m| m.len() == 1) else {
        return Err(format!(
            "a smart playlist rules object where operator '{op_key}' maps to an object with exactly one field (e.g. {{\"{op_key}\": {{\"genre\": \"Rock\"}}}})"
        ));
    };

    let (field, field_val) = field_map.iter().next().expect("length checked above");
    if field.trim().is_empty() {
        return Err(format!(
            "a smart playlist rules object where operator '{op_key}' uses a non-empty field name"
        ));
    }

    match op_lower {
        "intherange" => {
            if !field_val.as_array().is_some_and(|a| a.len() == 2) {
                return Err(format!(
                    "a smart playlist rules object where '{op_key}.{field}' is a 2-element [min, max] array"
                ));
            }
        }
        "inplaylist" | "notinplaylist" => {
            if field != "id" || !field_val.as_str().is_some_and(|s| !s.trim().is_empty()) {
                return Err(format!(
                    "a smart playlist rules object where '{op_key}' is {{\"id\": \"<playlist_id>\"}}"
                ));
            }
        }
        "inthelast" | "notinthelast" => {
            let valid = field_val.as_i64().is_some()
                || field_val
                    .as_str()
                    .is_some_and(|s| s.trim().parse::<i64>().is_ok());
            if !valid {
                return Err(format!(
                    "a smart playlist rules object where '{op_key}.{field}' is an integer number of days"
                ));
            }
        }
        _ => {
            if field_val.is_object() {
                return Err(format!(
                    "a smart playlist rules object where '{op_key}.{field}' is a scalar value or array, not an object"
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn accepts_valid_navidrome_criteria() {
        let cases = [
            json!({"all": [{"is": {"loved": true}}]}),
            json!({
                "any": [
                    {"is": {"genre": "Jazz"}},
                    {"contains": {"genre": "Bossa"}}
                ],
                "sort": "random",
                "order": "DESC",
                "limit": 50,
                "offset": 0
            }),
            json!({
                "all": [
                    {"contains": {"title": "love"}},
                    {"notContains": {"title": "hate"}},
                    {"any": [
                        {"isNot": {"artist": "u2"}},
                        {"is": {"album": "best of"}}
                    ]},
                    {"all": [
                        {"startsWith": {"comment": "this"}},
                        {"endsWith": {"filepath": ".flac"}},
                        {"inTheRange": {"year": [1980, 1990]}},
                        {"gt": {"rating": 3}},
                        {"lt": {"playCount": 10}},
                        {"before": {"lastPlayed": "2025-01-01"}},
                        {"after": {"dateAdded": "2024-01-01"}},
                        {"inTheLast": {"lastPlayed": 30}},
                        {"notInTheLast": {"dateAdded": 7}},
                        {"inPlaylist": {"id": "pl-1"}},
                        {"notInPlaylist": {"id": "pl-2"}},
                        {"is": {"library_id": [1, 2]}}
                    ]}
                ],
                "sort": "-dateAdded,title",
                "order": "asc",
                "limit": 20,
                "offset": 10
            }),
            // Case-insensitive operator names match Navidrome's json.go strings.ToLower(k)
            json!({"all": [{"isnot": {"genre": "Pop"}}]}),
        ];

        for (i, rules) in cases.iter().enumerate() {
            assert!(
                validate_rules(rules).is_ok(),
                "case {i} rejected: {:?}",
                validate_rules(rules)
            );
        }
    }

    #[test]
    fn rejects_invalid_top_level_shapes_and_keys() {
        for bad in [
            json!(null),
            json!("all"),
            json!([]),
            json!({}),
            json!({"sort": "title"}),
            json!({"all": []}),
            json!({"any": []}),
            json!({"all": [{"is": {"loved": true}}], "any": [{"is": {"rating": 5}}]}),
            json!({"all": [{"is": {"loved": true}}], "unknown": 1}),
            json!({"all": [{"is": {"loved": true}}], "sort": 123}),
            json!({"all": [{"is": {"loved": true}}], "order": "sideways"}),
            json!({"all": [{"is": {"loved": true}}], "order": true}),
            json!({"all": [{"is": {"loved": true}}], "limit": -1}),
            json!({"all": [{"is": {"loved": true}}], "limit": 1.5}),
            json!({"all": [{"is": {"loved": true}}], "limit": "50"}),
            json!({"all": [{"is": {"loved": true}}], "offset": -5}),
            json!({"all": [{"is": {"loved": true}}], "offset": "0"}),
        ] {
            assert!(validate_rules(&bad).is_err(), "expected error for {bad}");
        }
    }

    #[test]
    fn rejects_invalid_rules_and_operators() {
        for bad in [
            json!({"all": [{}]}),
            json!({"all": ["not an object"]}),
            json!({"all": [{"equals": {"title": "x"}}]}),
            json!({"all": [{"is": {}}]}),
            json!({"all": [{"is": {"title": "a", "artist": "b"}}]}),
            json!({"all": [{"is": {"": "a"}}]}),
            json!({"all": [{"is": "not an object"}]}),
            json!({"all": [{"is": {"title": {"nested": true}}}]}),
            json!({"all": [{"inTheRange": {"year": [1980]}}]}),
            json!({"all": [{"inTheRange": {"year": [1980, 1990, 2000]}}]}),
            json!({"all": [{"inTheRange": {"year": 1980}}]}),
            json!({"all": [{"inPlaylist": {"playlist": "pl-1"}}]}),
            json!({"all": [{"inPlaylist": {"id": ""}}]}),
            json!({"all": [{"notInPlaylist": {"id": 42}}]}),
            json!({"all": [{"inTheLast": {"lastPlayed": "thirty"}}]}),
            json!({"all": [{"notInTheLast": {"lastPlayed": 3.5}}]}),
            json!({"all": [{"any": []}]}),
        ] {
            assert!(validate_rules(&bad).is_err(), "expected error for {bad}");
        }
    }

    #[test]
    fn rejects_excessive_nesting_depth() {
        let mut rule = json!({"is": {"loved": true}});
        for _ in 0..20 {
            rule = json!({"all": [rule]});
        }
        assert!(validate_rules(&rule).is_err());
    }
}
