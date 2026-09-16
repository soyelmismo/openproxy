use serde_json::{Value, json};

pub fn clean_array_schema(arr: &mut [Value], is_schema_node: bool, depth: usize) {
    for item in arr.iter_mut() {
        super::clean_json_schema_recursive(item, is_schema_node, depth + 1);
    }
}

pub fn clean_object_schema(
    map: &mut serde_json::Map<String, Value>,
    is_schema_node: bool,
    depth: usize,
) -> bool {
    merge_all_of(map);
    normalize_object_schema(map);
    clean_object_properties_and_items(map, depth);
    super::sanitize::clean_unions_and_hints(map, depth);
    super::sanitize::sanitize_schema_fields(map, is_schema_node, depth)
}

fn merge_items_into_properties(properties_val: &mut Value, items_val: &mut Value) {
    if let (Some(target_map), Some(source_map)) =
        (properties_val.as_object_mut(), items_val.as_object_mut())
    {
        for (k, v) in std::mem::take(source_map) {
            target_map.entry(k).or_insert(v);
        }
    }
}

fn normalize_object_schema(map: &mut serde_json::Map<String, Value>) {
    let is_object_like = map.get("type").and_then(|t| t.as_str()) == Some("object")
        || map.contains_key("properties");
    if !is_object_like {
        return;
    }
    let Some(mut items) = map.remove("items") else {
        return;
    };

    tracing::warn!(
        "[Schema-Normalization] Found 'items' in an Object-like node. Moving content to 'properties'."
    );
    let target_props = map
        .entry("properties".to_string())
        .or_insert_with(|| json!({}));
    merge_items_into_properties(target_props, &mut items);
}

fn prune_invalid_properties(
    props: &mut serde_json::Map<String, Value>,
) -> std::collections::HashSet<String> {
    let mut dropped_keys = std::collections::HashSet::new();
    props.retain(|k, v| {
        if v.is_object() {
            true
        } else {
            dropped_keys.insert(k.clone());
            false
        }
    });
    dropped_keys
}

fn clean_and_collect_nullable(
    props: &mut serde_json::Map<String, Value>,
    depth: usize,
) -> std::collections::HashSet<String> {
    let mut nullable_keys = std::collections::HashSet::new();
    for (k, v) in props.iter_mut() {
        if super::clean_json_schema_recursive(v, true, depth + 1) {
            nullable_keys.insert(k.clone());
        }
    }
    nullable_keys
}

fn update_required_for_dropped_or_nullable(
    map: &mut serde_json::Map<String, Value>,
    dropped_keys: &std::collections::HashSet<String>,
    nullable_keys: &std::collections::HashSet<String>,
) {
    if nullable_keys.is_empty() && dropped_keys.is_empty() {
        return;
    }
    let Some(Value::Array(req_arr)) = map.get_mut("required") else {
        return;
    };
    req_arr.retain(|r| {
        r.as_str()
            .is_none_or(|s| !nullable_keys.contains(s) && !dropped_keys.contains(s))
    });
    if req_arr.is_empty() {
        map.remove("required");
    }
}

fn clean_properties(map: &mut serde_json::Map<String, Value>, depth: usize) {
    let Some(Value::Object(props)) = map.get_mut("properties") else {
        return;
    };

    let dropped_keys = prune_invalid_properties(props);
    let nullable_keys = clean_and_collect_nullable(props, depth);
    update_required_for_dropped_or_nullable(map, &dropped_keys, &nullable_keys);

    map.entry("type".to_string())
        .or_insert_with(|| Value::String("object".to_string()));
}

fn clean_items(map: &mut serde_json::Map<String, Value>, depth: usize) {
    if map.get("items").is_some_and(|i| !i.is_object()) {
        map.remove("items");
    }
    if let Some(items) = map.get_mut("items") {
        super::clean_json_schema_recursive(items, true, depth + 1);
        map.entry("type".to_string())
            .or_insert_with(|| Value::String("array".to_string()));
    }
}

fn clean_nested_non_schema_fields(map: &mut serde_json::Map<String, Value>, depth: usize) {
    if !map.contains_key("properties") && !map.contains_key("items") {
        for (k, v) in map.iter_mut() {
            if !matches!(k.as_str(), "anyOf" | "oneOf" | "allOf" | "enum" | "type") {
                super::clean_json_schema_recursive(v, false, depth + 1);
            }
        }
    }
}

fn clean_object_properties_and_items(map: &mut serde_json::Map<String, Value>, depth: usize) {
    clean_properties(map, depth);
    clean_items(map, depth);
    clean_nested_non_schema_fields(map, depth);
}

fn merge_sub_properties(
    sub_map: &mut serde_json::Map<String, Value>,
    merged_properties: &mut serde_json::Map<String, Value>,
) {
    let Some(Value::Object(props)) = sub_map.remove("properties") else {
        return;
    };
    for (k, v) in props {
        merged_properties.insert(k, v);
    }
}

fn merge_sub_required(
    sub_map: &mut serde_json::Map<String, Value>,
    merged_required: &mut std::collections::HashSet<String>,
) {
    let Some(Value::Array(reqs)) = sub_map.remove("required") else {
        return;
    };
    for req in reqs {
        if let Value::String(s) = req {
            merged_required.insert(s);
        }
    }
}

fn merge_sub_other_fields(
    sub_map: serde_json::Map<String, Value>,
    other_fields: &mut serde_json::Map<String, Value>,
) {
    for (k, v) in sub_map {
        if k != "allOf" && !other_fields.contains_key(&k) {
            other_fields.insert(k, v);
        }
    }
}

fn merge_all_of_sub_schema(
    mut sub_map: serde_json::Map<String, Value>,
    merged_properties: &mut serde_json::Map<String, Value>,
    merged_required: &mut std::collections::HashSet<String>,
    other_fields: &mut serde_json::Map<String, Value>,
) {
    merge_sub_properties(&mut sub_map, merged_properties);
    merge_sub_required(&mut sub_map, merged_required);
    merge_sub_other_fields(sub_map, other_fields);
}

fn merge_into_existing_properties(
    map: &mut serde_json::Map<String, Value>,
    merged_properties: serde_json::Map<String, Value>,
) {
    if merged_properties.is_empty() {
        return;
    }
    let existing_props = map
        .entry("properties".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Value::Object(existing_map) = existing_props {
        for (k, v) in merged_properties {
            existing_map.entry(k).or_insert(v);
        }
    }
}

fn merge_into_existing_required(
    map: &mut serde_json::Map<String, Value>,
    merged_required: std::collections::HashSet<String>,
) {
    if merged_required.is_empty() {
        return;
    }
    let existing_reqs = map
        .entry("required".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Value::Array(req_arr) = existing_reqs {
        let mut current_reqs: std::collections::HashSet<String> = req_arr
            .iter()
            .filter_map(|v| v.as_str().map(std::string::ToString::to_string))
            .collect();
        for req in merged_required {
            if current_reqs.insert(req.clone()) {
                req_arr.push(Value::String(req));
            }
        }
    }
}

fn apply_merged_all_of(
    map: &mut serde_json::Map<String, Value>,
    merged_properties: serde_json::Map<String, Value>,
    merged_required: std::collections::HashSet<String>,
    other_fields: serde_json::Map<String, Value>,
) {
    for (k, v) in other_fields {
        map.entry(k).or_insert(v);
    }
    merge_into_existing_properties(map, merged_properties);
    merge_into_existing_required(map, merged_required);
}

/// [NEW] 合并 allOf 数组中的所有子 Schema
pub fn merge_all_of(map: &mut serde_json::Map<String, Value>) {
    let Some(Value::Array(all_of)) = map.remove("allOf") else {
        return;
    };
    let mut merged_properties = serde_json::Map::new();
    let mut merged_required = std::collections::HashSet::new();
    let mut other_fields = serde_json::Map::new();

    for sub_schema in all_of {
        if let Value::Object(sub_map) = sub_schema {
            merge_all_of_sub_schema(
                sub_map,
                &mut merged_properties,
                &mut merged_required,
                &mut other_fields,
            );
        }
    }

    apply_merged_all_of(map, merged_properties, merged_required, other_fields);
}
