use serde_json::Value;

pub const MAX_RECURSION_DEPTH: usize = 10;

fn extract_defs_from_key(
    map: &serde_json::Map<String, Value>,
    key: &str,
    defs: &mut serde_json::Map<String, Value>,
) {
    if let Some(Value::Object(d)) = map.get(key) {
        for (k, v) in d {
            defs.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
}

fn collect_defs_from_object(
    map: &serde_json::Map<String, Value>,
    defs: &mut serde_json::Map<String, Value>,
) {
    extract_defs_from_key(map, "$defs", defs);
    extract_defs_from_key(map, "definitions", defs);
    for (key, v) in map {
        if key != "$defs" && key != "definitions" {
            collect_all_defs(v, defs);
        }
    }
}

/// [NEW #952] 递归收集所有层级的 $defs 和 definitions
///
/// MCP 工具的 schema 可能在任意嵌套层级定义 $defs，而非仅在根层级。
/// 此函数深度遍历整个 schema，收集所有定义到统一的 map 中。
pub fn collect_all_defs(value: &Value, defs: &mut serde_json::Map<String, Value>) {
    match value {
        Value::Object(map) => collect_defs_from_object(map, defs),
        Value::Array(arr) => {
            for item in arr {
                collect_all_defs(item, defs);
            }
        }
        _ => {}
    }
}

fn fallback_unresolved_ref(map: &mut serde_json::Map<String, Value>, ref_path: &str) {
    map.insert("type".to_string(), serde_json::json!("string"));
    if !map.contains_key("description") {
        map.insert(
            "description".to_string(),
            Value::String(String::with_capacity(32 + ref_path.len())),
        );
    }
    if let Some(Value::String(s)) = map.get_mut("description")
        && !s.contains(ref_path)
    {
        if !s.is_empty() {
            s.push(' ');
        }
        use std::fmt::Write;
        let _ = write!(s, "(Unresolved $ref: {ref_path})");
    }
}

fn resolve_ref_path(
    map: &mut serde_json::Map<String, Value>,
    defs: &serde_json::Map<String, Value>,
    ref_path: &str,
    depth: usize,
) {
    let ref_name = ref_path.split('/').next_back().unwrap_or(ref_path);

    let Some(Value::Object(def_map)) = defs.get(ref_name) else {
        fallback_unresolved_ref(map, ref_path);
        return;
    };

    for (k, v) in def_map {
        if !map.contains_key(k) {
            map.insert(k.clone(), v.clone());
        }
    }
    flatten_refs(map, defs, depth + 1);
}

fn flatten_refs_in_children(
    map: &mut serde_json::Map<String, Value>,
    defs: &serde_json::Map<String, Value>,
    depth: usize,
) {
    for (_, v) in map.iter_mut() {
        match v {
            Value::Object(child_map) => flatten_refs(child_map, defs, depth + 1),
            Value::Array(arr) => {
                for item in arr {
                    if let Value::Object(item_map) = item {
                        flatten_refs(item_map, defs, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }
}

/// 递归展开 $ref
pub fn flatten_refs(
    map: &mut serde_json::Map<String, Value>,
    defs: &serde_json::Map<String, Value>,
    depth: usize,
) {
    if depth > MAX_RECURSION_DEPTH {
        tracing::warn!("[Schema-Flatten] Max recursion depth reached, stopping ref expansion.");
        return;
    }

    if let Some(Value::String(ref_path)) = map.remove("$ref") {
        resolve_ref_path(map, defs, &ref_path, depth);
    }

    flatten_refs_in_children(map, defs, depth);
}
