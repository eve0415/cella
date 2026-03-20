use std::collections::HashSet;

/// Extend a `Vec<String>` with new items, skipping duplicates.
pub(super) fn extend_dedup(target: &mut Vec<String>, items: &[String]) {
    let existing: HashSet<String> = target.iter().cloned().collect();
    for item in items {
        if !existing.contains(item) {
            target.push(item.clone());
        }
    }
}

/// Deep merge two JSON values. `overlay` values override `base` values for
/// the same key; objects are merged recursively; non-object values are replaced.
pub(super) fn deep_merge(
    base: &serde_json::Value,
    overlay: &serde_json::Value,
) -> serde_json::Value {
    match (base, overlay) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(overlay_map)) => {
            let mut merged = base_map.clone();
            for (k, v) in overlay_map {
                let entry = merged.entry(k.clone()).or_insert(serde_json::Value::Null);
                *entry = deep_merge(entry, v);
            }
            serde_json::Value::Object(merged)
        }
        // Non-object: overlay wins.
        (_, overlay) => overlay.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn deep_merge_nested_objects() {
        let base = json!({ "a": { "b": 1, "c": 2 } });
        let overlay = json!({ "a": { "c": 3, "d": 4 } });

        let result = deep_merge(&base, &overlay);

        assert_eq!(result, json!({ "a": { "b": 1, "c": 3, "d": 4 } }));
    }

    #[test]
    fn deep_merge_overlay_replaces_non_object() {
        let base = json!({ "key": "old" });
        let overlay = json!({ "key": "new" });

        let result = deep_merge(&base, &overlay);

        assert_eq!(result, json!({ "key": "new" }));
    }

    #[test]
    fn deep_merge_base_null() {
        let base = serde_json::Value::Null;
        let overlay = json!({ "key": "value" });

        let result = deep_merge(&base, &overlay);

        assert_eq!(result, json!({ "key": "value" }));
    }
}
