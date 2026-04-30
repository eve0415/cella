use serde_json::Value;

/// Deep-merge `overlay` into `base`.
///
/// - Objects: recursively merge matching keys; new keys inserted.
/// - Arrays: overlay items are prepended (higher-priority layers match first
///   in first-match rule engines like network rules).
/// - Scalars: overlay replaces base.
pub fn deep_merge(base: &mut Value, overlay: &Value) {
    match (base.as_object_mut(), overlay.as_object()) {
        (Some(base_obj), Some(overlay_obj)) => {
            for (key, overlay_value) in overlay_obj {
                if let Some(base_value) = base_obj.get_mut(key) {
                    if let (Some(base_arr), Some(overlay_arr)) =
                        (base_value.as_array_mut(), overlay_value.as_array())
                    {
                        let mut merged = overlay_arr.clone();
                        merged.append(base_arr);
                        *base_arr = merged;
                    } else {
                        deep_merge(base_value, overlay_value);
                    }
                } else {
                    base_obj.insert(key.clone(), overlay_value.clone());
                }
            }
        }
        _ => {
            *base = overlay.clone();
        }
    }
}

pub fn merge_layers(layers: &[Value]) -> Value {
    let mut result = Value::Object(serde_json::Map::new());
    for layer in layers {
        deep_merge(&mut result, layer);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scalar_override() {
        let mut base = json!({"a": 1});
        deep_merge(&mut base, &json!({"a": 2}));
        assert_eq!(base["a"], 2);
    }

    #[test]
    fn nested_object_merge() {
        let mut base = json!({"a": {"x": 1, "y": 2}});
        deep_merge(&mut base, &json!({"a": {"y": 3, "z": 4}}));
        assert_eq!(base["a"]["x"], 1);
        assert_eq!(base["a"]["y"], 3);
        assert_eq!(base["a"]["z"], 4);
    }

    #[test]
    fn array_prepend_overlay() {
        let mut base = json!({"a": [1, 2]});
        deep_merge(&mut base, &json!({"a": [3, 4]}));
        assert_eq!(base["a"], json!([3, 4, 1, 2]));
    }

    #[test]
    fn new_key_added() {
        let mut base = json!({"a": 1});
        deep_merge(&mut base, &json!({"b": 2}));
        assert_eq!(base["a"], 1);
        assert_eq!(base["b"], 2);
    }

    #[test]
    fn null_override() {
        let mut base = json!({"a": 1});
        deep_merge(&mut base, &json!({"a": null}));
        assert_eq!(base["a"], json!(null));
    }

    #[test]
    fn type_change_overlay_wins() {
        let mut base = json!({"a": "string"});
        deep_merge(&mut base, &json!({"a": 42}));
        assert_eq!(base["a"], 42);
    }

    #[test]
    fn deeply_nested() {
        let mut base = json!({"a": {"b": {"c": 1, "d": 2}}});
        deep_merge(&mut base, &json!({"a": {"b": {"c": 10, "e": 3}}}));
        assert_eq!(base["a"]["b"]["c"], 10);
        assert_eq!(base["a"]["b"]["d"], 2);
        assert_eq!(base["a"]["b"]["e"], 3);
    }

    #[test]
    fn empty_overlay_noop() {
        let mut base = json!({"a": 1});
        deep_merge(&mut base, &json!({}));
        assert_eq!(base, json!({"a": 1}));
    }

    #[test]
    fn empty_base_gets_overlay() {
        let mut base = json!({});
        deep_merge(&mut base, &json!({"a": 1}));
        assert_eq!(base, json!({"a": 1}));
    }

    #[test]
    fn non_object_base_replaced() {
        let mut base = json!("string");
        deep_merge(&mut base, &json!({"a": 1}));
        assert_eq!(base, json!({"a": 1}));
    }

    #[test]
    fn merge_layers_three_layers() {
        let layers = vec![
            json!({"a": 1, "b": {"x": 10}}),
            json!({"b": {"y": 20}}),
            json!({"a": 3, "b": {"x": 30}}),
        ];
        let result = merge_layers(&layers);
        assert_eq!(result["a"], 3);
        assert_eq!(result["b"]["x"], 30);
        assert_eq!(result["b"]["y"], 20);
    }

    #[test]
    fn merge_layers_empty() {
        let result = merge_layers(&[]);
        assert_eq!(result, json!({}));
    }

    #[test]
    fn merge_layers_single() {
        let result = merge_layers(&[json!({"a": 1})]);
        assert_eq!(result, json!({"a": 1}));
    }

    #[test]
    fn array_concat_across_layers() {
        let layers = vec![
            json!({"rules": [{"domain": "a.com"}]}),
            json!({"rules": [{"domain": "b.com"}]}),
            json!({"rules": [{"domain": "c.com"}]}),
        ];
        let result = merge_layers(&layers);
        let rules = result["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0]["domain"], "c.com");
        assert_eq!(rules[1]["domain"], "b.com");
        assert_eq!(rules[2]["domain"], "a.com");
    }

    #[test]
    fn mixed_nested_arrays_and_scalars() {
        let mut base = json!({
            "tools": {
                "claude-code": {"enabled": true, "version": "latest"},
                "codex": {"enabled": true}
            },
            "network": {"rules": [{"domain": "a.com"}]}
        });
        let overlay = json!({
            "tools": {
                "claude-code": {"version": "stable"},
                "nvim": {"forward_config": true}
            },
            "network": {"rules": [{"domain": "b.com"}]}
        });
        deep_merge(&mut base, &overlay);
        assert_eq!(base["tools"]["claude-code"]["enabled"], true);
        assert_eq!(base["tools"]["claude-code"]["version"], "stable");
        assert_eq!(base["tools"]["codex"]["enabled"], true);
        assert_eq!(base["tools"]["nvim"]["forward_config"], true);
        assert_eq!(base["network"]["rules"].as_array().unwrap().len(), 2);
    }
}
