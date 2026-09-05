use serde_json::{Value, json};

/// 不被 Gemini 支持但包含重要语义信息的约束字段
/// 这些字段将在删除前被转化为 description 提示
const CONSTRAINT_FIELDS: &[(&str, &str)] = &[
    ("minLength", "minLen"),
    ("maxLength", "maxLen"),
    ("pattern", "pattern"),
    ("minimum", "min"),
    ("maximum", "max"),
    ("multipleOf", "multipleOf"),
    ("exclusiveMinimum", "exclMin"),
    ("exclusiveMaximum", "exclMax"),
    ("minItems", "minItems"),
    ("maxItems", "maxItems"),
    ("format", "format"),
];

const MAX_RECURSION_DEPTH: usize = 10;

/// 递归清理 JSON Schema 以符合 Gemini 接口要求
///
/// 1. [New] 展开 $ref 和 $defs: 将引用替换为实际定义，解决 Gemini 不支持 $ref 的问题
/// 2. 移除不支持的字段: $schema, additionalProperties, format, default, uniqueItems, validation fields
/// 3. 处理联合类型: ["string", "null"] -> "string"
/// 4. [NEW] 处理 anyOf 联合类型: anyOf: [{"type": "string"}, {"type": "null"}] -> "type": "string"
/// 5. 将 type 字段的值转换为小写 (Gemini v1internal 要求)
/// 6. 移除数字校验字段: multipleOf, exclusiveMinimum, exclusiveMaximum 等
pub fn clean_json_schema(value: &mut Value) {
    // 0. 预处理：展开 $ref (Schema Flattening)
    // [FIX #952] 递归收集所有层级的 $defs/definitions，而非仅从根层级提取
    let mut all_defs = serde_json::Map::new();
    collect_all_defs(value, &mut all_defs);

    // 移除根层级的 $defs/definitions (保持向后兼容)
    if let Value::Object(map) = value {
        map.remove("$defs");
        map.remove("definitions");
    }

    // [FIX #952] 始终运行 flatten_refs，即使 defs 为空
    // 这样可以捕获并处理无法解析的 $ref (降级为 string 类型)
    if let Value::Object(map) = value {
        flatten_refs(map, &all_defs, 0);
    }

    // 递归清理
    clean_json_schema_recursive(value, true, 0);
}

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
fn collect_all_defs(value: &Value, defs: &mut serde_json::Map<String, Value>) {
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
    #[allow(clippy::collapsible_if)]
    if let Some(Value::String(s)) = map.get_mut("description") {
        if !s.contains(ref_path) {
            if !s.is_empty() {
                s.push(' ');
            }
            use std::fmt::Write;
            let _ = write!(s, "(Unresolved $ref: {ref_path})");
        }
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
fn flatten_refs(
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

fn clean_json_schema_recursive(value: &mut Value, is_schema_node: bool, depth: usize) -> bool {
    if depth > MAX_RECURSION_DEPTH {
        debug_assert!(
            false,
            "Max recursion depth reached in clean_json_schema_recursive"
        );
        return false;
    }

    match value {
        Value::Object(map) => clean_object_schema(map, is_schema_node, depth),
        Value::Array(arr) => {
            clean_array_schema(arr, is_schema_node, depth);
            false
        }
        _ => false,
    }
}

fn clean_array_schema(arr: &mut [Value], is_schema_node: bool, depth: usize) {
    for item in arr.iter_mut() {
        clean_json_schema_recursive(item, is_schema_node, depth + 1);
    }
}

fn clean_object_schema(
    map: &mut serde_json::Map<String, Value>,
    is_schema_node: bool,
    depth: usize,
) -> bool {
    merge_all_of(map);
    normalize_object_schema(map);
    clean_object_properties_and_items(map, depth);
    clean_unions_and_hints(map, depth);
    sanitize_schema_fields(map, is_schema_node, depth)
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
        if clean_json_schema_recursive(v, true, depth + 1) {
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
        clean_json_schema_recursive(items, true, depth + 1);
        map.entry("type".to_string())
            .or_insert_with(|| Value::String("array".to_string()));
    }
}

fn clean_nested_non_schema_fields(map: &mut serde_json::Map<String, Value>, depth: usize) {
    if !map.contains_key("properties") && !map.contains_key("items") {
        for (k, v) in map.iter_mut() {
            if !matches!(k.as_str(), "anyOf" | "oneOf" | "allOf" | "enum" | "type") {
                clean_json_schema_recursive(v, false, depth + 1);
            }
        }
    }
}

fn clean_object_properties_and_items(map: &mut serde_json::Map<String, Value>, depth: usize) {
    clean_properties(map, depth);
    clean_items(map, depth);
    clean_nested_non_schema_fields(map, depth);
}

fn clean_union_branches(map: &mut serde_json::Map<String, Value>, depth: usize) {
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(arr)) = map.get_mut(key) {
            for branch in arr.iter_mut() {
                clean_json_schema_recursive(branch, true, depth + 1);
            }
        }
    }
}

fn merge_union_properties(map: &mut serde_json::Map<String, Value>, v: Value) {
    if let (Some(target_props), Value::Object(source_props)) = (
        map.entry("properties".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut(),
        v,
    ) {
        for (pk, pv) in source_props {
            target_props.entry(pk).or_insert(pv);
        }
    }
}

fn merge_union_required(map: &mut serde_json::Map<String, Value>, v: Value) {
    if let (Some(target_req), Value::Array(source_req)) = (
        map.entry("required".to_string())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut(),
        v,
    ) {
        let mut seen: std::collections::HashSet<Value> = target_req.iter().cloned().collect();
        for rv in source_req {
            if seen.insert(rv.clone()) {
                target_req.push(rv);
            }
        }
    }
}

fn merge_union_branch(
    map: &mut serde_json::Map<String, Value>,
    branch_obj: serde_json::Map<String, Value>,
) {
    for (k, v) in branch_obj {
        match k.as_str() {
            "properties" => merge_union_properties(map, v),
            "required" => merge_union_required(map, v),
            _ => {
                map.entry(k).or_insert(v);
            }
        }
    }
}

fn apply_union_type_hints(map: &mut serde_json::Map<String, Value>, all_types: &[String]) {
    if all_types.len() > 1 {
        let type_hint = format!("Accepts: {}", all_types.join(" | "));
        append_hint_to_description(map, &type_hint);
    }
}

fn clean_unions_and_hints(map: &mut serde_json::Map<String, Value>, depth: usize) {
    clean_union_branches(map, depth);

    let union_to_merge = map
        .get("anyOf")
        .or_else(|| map.get("oneOf"))
        .and_then(|v| v.as_array())
        .map(|arr| arr.as_slice());

    let Some(union_array) = union_to_merge else {
        return;
    };
    let Some((best_branch, all_types)) = extract_best_schema_from_union(union_array) else {
        return;
    };

    if let Value::Object(branch_obj) = best_branch {
        merge_union_branch(map, branch_obj);
    }
    apply_union_type_hints(map, &all_types);
}

fn wrap_bare_properties_node(map: &mut serde_json::Map<String, Value>, depth: usize) {
    let properties = std::mem::take(map);
    map.insert("type".to_string(), Value::String("object".to_string()));
    map.insert("properties".to_string(), Value::Object(properties));

    if let Some(Value::Object(props_map)) = map.get_mut("properties") {
        for v in props_map.values_mut() {
            clean_json_schema_recursive(v, true, depth + 1);
        }
    }
}

fn sanitize_required_fields(map: &mut serde_json::Map<String, Value>) {
    let Some(mut required_val) = map.remove("required") else {
        return;
    };
    if let Some(req_arr) = required_val.as_array_mut() {
        if let Some(props) = map.get("properties").and_then(|p| p.as_object()) {
            req_arr.retain(|k| k.as_str().is_some_and(|s| props.contains_key(s)));
        } else {
            req_arr.clear();
        }
    }
    map.insert("required".to_string(), required_val);
}

fn infer_schema_type(map: &serde_json::Map<String, Value>) -> &'static str {
    if map.contains_key("properties") {
        "object"
    } else if map.contains_key("items") {
        "array"
    } else {
        "string"
    }
}

fn inspect_type_name(s: &str, is_nullable: &mut bool, selected_type: &mut Option<String>) {
    let lower = s.to_lowercase();
    if lower == "null" {
        *is_nullable = true;
    } else if selected_type.is_none() {
        *selected_type = Some(lower);
    }
}

fn resolve_type_and_nullability(type_val: &Value, fallback: &str) -> (String, bool) {
    let mut is_nullable = false;
    let mut selected_type = None;

    match type_val {
        Value::String(s) => inspect_type_name(s, &mut is_nullable, &mut selected_type),
        Value::Array(arr) => {
            for item in arr.iter().filter_map(|i| i.as_str()) {
                inspect_type_name(item, &mut is_nullable, &mut selected_type);
            }
        }
        _ => {}
    }

    let final_type = selected_type.unwrap_or_else(|| fallback.to_string());
    (final_type, is_nullable)
}

fn normalize_type_field(map: &mut serde_json::Map<String, Value>) -> bool {
    if !map.contains_key("type") {
        let default_type = if map.contains_key("enum") {
            "string"
        } else {
            infer_schema_type(map)
        };
        map.insert("type".to_string(), Value::String(default_type.to_string()));
    }

    let fallback = infer_schema_type(map);
    let Some(type_val) = map.get_mut("type") else {
        return false;
    };

    let (resolved_type, is_nullable) = resolve_type_and_nullability(type_val, fallback);
    *type_val = Value::String(resolved_type);
    is_nullable
}

fn append_nullable_description(map: &mut serde_json::Map<String, Value>) {
    let desc_val = map
        .entry("description".to_string())
        .or_insert_with(|| Value::String(String::new()));
    if let Value::String(s) = desc_val
        && !s.contains("nullable")
    {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str("(nullable)");
    }
}

fn normalize_enum_items(map: &mut serde_json::Map<String, Value>) {
    let Some(Value::Array(arr)) = map.get_mut("enum") else {
        return;
    };
    for item in arr {
        if !item.is_string() {
            *item = Value::String(if item.is_null() {
                "null".to_string()
            } else {
                item.to_string()
            });
        }
    }
}

fn is_standard_keyword(k: &str) -> bool {
    matches!(
        k,
        "type" | "description" | "properties" | "required" | "items" | "enum" | "title"
    )
}

fn has_standard_keyword(map: &serde_json::Map<String, Value>) -> bool {
    map.keys().any(|k| is_standard_keyword(k.as_str()))
}

fn is_not_schema_payload(map: &serde_json::Map<String, Value>) -> bool {
    map.contains_key("functionCall") || map.contains_key("functionResponse")
}

fn ensure_object_properties(map: &mut serde_json::Map<String, Value>) {
    if map.get("type").and_then(|t| t.as_str()) == Some("object") && !map.contains_key("properties")
    {
        map.insert("properties".to_string(), serde_json::json!({}));
    }
}

fn sanitize_schema_fields(
    map: &mut serde_json::Map<String, Value>,
    is_schema_node: bool,
    depth: usize,
) -> bool {
    let has_std_kw = has_standard_keyword(map);
    let is_not_payload = is_not_schema_payload(map);

    if is_schema_node && !has_std_kw && !map.is_empty() && !is_not_payload {
        wrap_bare_properties_node(map, depth);
    }

    let looks_like_schema = (is_schema_node || has_std_kw) && !is_not_payload;
    if !looks_like_schema {
        return false;
    }

    move_constraints_to_description(map);
    map.retain(|k, _| is_standard_keyword(k.as_str()));

    ensure_object_properties(map);

    sanitize_required_fields(map);
    let is_effectively_nullable = normalize_type_field(map);

    if is_effectively_nullable {
        append_nullable_description(map);
    }

    normalize_enum_items(map);

    is_effectively_nullable
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
fn merge_all_of(map: &mut serde_json::Map<String, Value>) {
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

/// [NEW] 将提示信息追加到 description 字段
/// 参考 CLIProxyAPI 的 Lazy Hint 策略
fn append_hint_to_description(map: &mut serde_json::Map<String, Value>, hint: &str) {
    let desc_val = map
        .entry("description".to_string())
        .or_insert_with(|| Value::String(String::new()));

    if let Value::String(s) = desc_val {
        if s.is_empty() {
            *s = hint.to_string();
        } else if !s.contains(hint) {
            *s = format!("{s} {hint}");
        }
    }
}

fn extract_constraint_hint(
    map: &serde_json::Map<String, Value>,
    field: &str,
    label: &str,
) -> Option<String> {
    let val = map.get(field)?;
    (!val.is_null()).then(|| {
        let val_str = val
            .as_str()
            .map_or_else(|| val.to_string(), std::string::ToString::to_string);
        format!("{label}: {val_str}")
    })
}

/// [NEW] 将约束字段转化为 description 提示
/// 在删除约束字段前,将其语义信息保留在描述中,让模型能够理解约束
fn move_constraints_to_description(map: &mut serde_json::Map<String, Value>) {
    let hints: Vec<String> = CONSTRAINT_FIELDS
        .iter()
        .filter_map(|(field, label)| extract_constraint_hint(map, field, label))
        .collect();

    if !hints.is_empty() {
        let constraint_hint = format!("[Constraint: {}]", hints.join(", "));
        append_hint_to_description(map, &constraint_hint);
    }
}

/// [NEW] 计算 Schema 分支的复杂度得分 (用于 anyOf/oneOf 择优)
/// 评分标准: Object (3) > Array (2) > Scalar (1) > Null (0)
fn score_schema_option(val: &Value) -> i32 {
    let Some(obj) = val.as_object() else {
        return 0;
    };
    let type_str = obj.get("type").and_then(|t| t.as_str());
    if obj.contains_key("properties") || type_str == Some("object") {
        3
    } else if obj.contains_key("items") || type_str == Some("array") {
        2
    } else {
        i32::from(type_str.is_some_and(|t| t != "null"))
    }
}

/// [NEW] 从 anyOf/oneOf 联合类型数组中选取最佳非 null Schema 分支
/// 返回: (最佳Schema, 所有可能的类型列表)
/// 参考 CLIProxyAPI 的 selectBest 逻辑
fn extract_best_schema_from_union(union_array: &[Value]) -> Option<(Value, Vec<String>)> {
    let mut best_option: Option<&Value> = None;
    let mut best_score = -1;
    let mut all_types = Vec::new();

    for item in union_array {
        let score = score_schema_option(item);

        // 收集类型信息
        if let Some(type_str) = get_schema_type_name(item)
            && !all_types.contains(&type_str)
        {
            all_types.push(type_str);
        }

        if score > best_score {
            best_score = score;
            best_option = Some(item);
        }
    }

    best_option.cloned().map(|schema| (schema, all_types))
}

/// [NEW] 获取 Schema 的类型名称
fn get_schema_type_name(schema: &Value) -> Option<String> {
    let obj = schema.as_object()?;
    if let Some(t) = obj.get("type").and_then(|t| t.as_str()) {
        return Some(t.to_string());
    }
    if obj.contains_key("properties") {
        return Some("object".to_string());
    }
    if obj.contains_key("items") {
        return Some("array".to_string());
    }
    None
}

/// 修正工具调用参数的类型，使其符合 schema 定义
///
/// 根据 schema 中的 type 定义，自动转换参数值的类型：
/// - "123" → 123 (string → number/integer)
/// - "true" → true (string → boolean)
/// - 123 → "123" (number → string)
///
/// # Arguments
/// * `args` - 工具调用的参数对象 (会被原地修改)
/// * `schema` - 工具的参数 schema 定义 (通常是 parameters 对象)
pub fn fix_tool_call_args(args: &mut Value, schema: &Value) {
    if let Some(properties) = schema.get("properties").and_then(|p| p.as_object())
        && let Some(args_obj) = args.as_object_mut()
    {
        for (key, value) in args_obj.iter_mut() {
            if let Some(prop_schema) = properties.get(key) {
                fix_single_arg_recursive(value, prop_schema);
            }
        }
    }
}

fn fix_object_arg(value: &mut Value, nested_props: &serde_json::Map<String, Value>) {
    let Some(value_obj) = value.as_object_mut() else {
        return;
    };
    for (key, nested_value) in value_obj.iter_mut() {
        if let Some(nested_schema) = nested_props.get(key) {
            fix_single_arg_recursive(nested_value, nested_schema);
        }
    }
}

fn fix_array_arg(value: &mut Value, items_schema: &Value) {
    let Some(arr) = value.as_array_mut() else {
        return;
    };
    for item in arr {
        fix_single_arg_recursive(item, items_schema);
    }
}

fn is_preserved_string_number(s: &str) -> bool {
    s.starts_with('0') && s.len() > 1 && !s.starts_with("0.")
}

fn coerce_to_number(value: &mut Value) {
    let Some(s) = value.as_str() else {
        return;
    };
    // [SAFETY] 保护具有前导零的版本号或代码 (如 "01", "007")，不应转为数字
    if is_preserved_string_number(s) {
        return;
    }

    if let Ok(i) = s.parse::<i64>() {
        *value = Value::Number(serde_json::Number::from(i));
    } else if let Some(n) = s.parse::<f64>().ok().and_then(serde_json::Number::from_f64) {
        *value = Value::Number(n);
    }
}

fn coerce_str_to_boolean(s: &str) -> Option<bool> {
    match s.to_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn coerce_to_boolean(value: &mut Value) {
    if let Some(s) = value.as_str() {
        if let Some(b) = coerce_str_to_boolean(s) {
            *value = Value::Bool(b);
        }
    } else if let Some(n) = value.as_i64() {
        if n == 1 {
            *value = Value::Bool(true);
        } else if n == 0 {
            *value = Value::Bool(false);
        }
    }
}

fn coerce_to_string(value: &mut Value) {
    // 非字符串 → 字符串 (防止客户端误传数字给文本字段)
    if !value.is_string() && !value.is_null() && !value.is_object() && !value.is_array() {
        *value = Value::String(value.to_string());
    }
}

fn fix_scalar_arg(value: &mut Value, schema_type: &str) {
    match schema_type {
        "number" | "integer" => coerce_to_number(value),
        "boolean" => coerce_to_boolean(value),
        "string" => coerce_to_string(value),
        _ => {}
    }
}

/// 递归修正单个参数的类型
fn fix_single_arg_recursive(value: &mut Value, schema: &Value) {
    // 1. 处理嵌套对象 (properties)
    if let Some(nested_props) = schema.get("properties").and_then(|p| p.as_object()) {
        fix_object_arg(value, nested_props);
        return;
    }

    // 2. 处理数组 (items)
    let schema_type = schema
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_lowercase();
    if schema_type == "array" {
        if let Some(items_schema) = schema.get("items") {
            fix_array_arg(value, items_schema);
        }
        return;
    }

    // 3. 处理基础类型修正
    fix_scalar_arg(value, schema_type.as_str());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_drops_boolean_subschemas() {
        // JSON Schema permits boolean sub-schemas (`prop: true|false`), but Gemini's
        // Schema proto rejects a non-object property value with HTTP 400. They must be
        // stripped at every depth (including inside `items`).
        let mut schema = json!({
            "type": "object",
            "properties": {
                "outer": {
                    "type": "object",
                    "properties": {
                        "forbidden": false,
                        "allowed": { "type": "string" }
                    },
                    "required": ["forbidden", "allowed"]
                },
                "list": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "nope": false,
                            "ok": { "type": "number" }
                        }
                    }
                }
            }
        });
        clean_json_schema(&mut schema);

        let outer_props = &schema["properties"]["outer"]["properties"];
        assert!(
            outer_props.get("forbidden").is_none(),
            "boolean sub-schema must be dropped"
        );
        assert!(
            outer_props["allowed"].is_object(),
            "valid sibling must survive"
        );

        let item_props = &schema["properties"]["list"]["items"]["properties"];
        assert!(
            item_props.get("nope").is_none(),
            "nested boolean sub-schema must be dropped"
        );
        assert!(item_props["ok"].is_object());

        let req = schema["properties"]["outer"]["required"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(req.iter().all(|r| r.as_str() != Some("forbidden")));
    }
    #[test]
    fn test_clean_json_schema_draft_2020_12() {
        let mut schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "location": {
                    "type": "string",
                    "minLength": 1,
                    "format": "city"
                },
                // 模拟属性名冲突：pattern 是一个 Object 属性，不应被移除
                "pattern": {
                    "type": "object",
                    "properties": {
                        "regex": { "type": "string", "pattern": "^[a-z]+$" }
                    }
                },
                "unit": {
                    "type": ["string", "null"],
                    "default": "celsius"
                }
            },
            "required": ["location"]
        });

        clean_json_schema(&mut schema);

        // 1. 验证类型保持小写
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["location"]["type"], "string");

        // 2. 验证标准字段被移除并转为描述 (Robust Constraint Migration)
        assert!(schema["properties"]["location"].get("minLength").is_none());
        assert!(schema["properties"]["location"].get("format").is_none());
        assert!(
            schema["properties"]["location"]["description"]
                .as_str()
                .unwrap()
                .contains("[Constraint: minLen: 1, format: city]")
        );

        // 3. 验证名为 "pattern" 的属性未被误删
        assert!(schema["properties"].get("pattern").is_some());
        assert_eq!(schema["properties"]["pattern"]["type"], "object");

        // 4. 验证内部的 pattern 校验字段被移除并转为描述
        assert!(
            schema["properties"]["pattern"]["properties"]["regex"]
                .get("pattern")
                .is_none()
        );
        assert!(
            schema["properties"]["pattern"]["properties"]["regex"]["description"]
                .as_str()
                .unwrap()
                .contains("[Constraint: pattern: ^[a-z]+$]")
        );

        // 5. 验证联合类型被降级为单一类型 (Protobuf 兼容性)
        assert_eq!(schema["properties"]["unit"]["type"], "string");

        // 6. 验证元数据字段被移除
        assert!(schema.get("$schema").is_none());
    }

    #[test]
    fn test_type_fallback() {
        // Test ["string", "null"] -> "string"
        let mut s1 = json!({"type": ["string", "null"]});
        clean_json_schema(&mut s1);
        assert_eq!(s1["type"], "string");

        // Test ["integer", "null"] -> "integer" (and lowercase check if needed, though usually integer)
        let mut s2 = json!({"type": ["integer", "null"]});
        clean_json_schema(&mut s2);
        assert_eq!(s2["type"], "integer");
    }

    #[test]
    fn test_flatten_refs() {
        let mut schema = json!({
            "$defs": {
                "Address": {
                    "type": "object",
                    "properties": {
                        "city": { "type": "string" }
                    }
                }
            },
            "properties": {
                "home": { "$ref": "#/$defs/Address" }
            }
        });

        clean_json_schema(&mut schema);

        // 验证引用被展开且类型转为小写
        assert_eq!(schema["properties"]["home"]["type"], "object");
        assert_eq!(
            schema["properties"]["home"]["properties"]["city"]["type"],
            "string"
        );
    }

    #[test]
    fn test_clean_json_schema_missing_required() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "existing_prop": { "type": "string" }
            },
            "required": ["existing_prop", "missing_prop"]
        });

        clean_json_schema(&mut schema);

        // 验证 missing_prop 被从 required 中移除
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].as_str().unwrap(), "existing_prop");
    }

    // [NEW TEST] 验证 anyOf 类型提取
    #[test]
    fn test_anyof_type_extraction() {
        // 测试 FastMCP 风格的 Optional[str] schema
        let mut schema = json!({
            "type": "object",
            "properties": {
                "testo": {
                    "anyOf": [
                        {"type": "string"},
                        {"type": "null"}
                    ],
                    "default": null,
                    "title": "Testo"
                },
                "importo": {
                    "anyOf": [
                        {"type": "number"},
                        {"type": "null"}
                    ],
                    "default": null,
                    "title": "Importo"
                },
                "attivo": {
                    "type": "boolean",
                    "title": "Attivo"
                }
            }
        });

        clean_json_schema(&mut schema);

        // 验证 anyOf 被移除
        assert!(schema["properties"]["testo"].get("anyOf").is_none());
        assert!(schema["properties"]["importo"].get("anyOf").is_none());

        // 验证 type 被正确提取
        assert_eq!(schema["properties"]["testo"]["type"], "string");
        assert_eq!(schema["properties"]["importo"]["type"], "number");
        assert_eq!(schema["properties"]["attivo"]["type"], "boolean");

        // 验证 default 被移除 (白名单之外)
        assert!(schema["properties"]["testo"].get("default").is_none());
    }

    // [NEW TEST] 验证 oneOf 类型提取
    #[test]
    fn test_oneof_type_extraction() {
        let mut schema = json!({
            "properties": {
                "value": {
                    "oneOf": [
                        {"type": "integer"},
                        {"type": "null"}
                    ]
                }
            }
        });

        clean_json_schema(&mut schema);

        assert!(schema["properties"]["value"].get("oneOf").is_none());
        assert_eq!(schema["properties"]["value"]["type"], "integer");
    }

    // [NEW TEST] 验证已有 type 不被覆盖
    #[test]
    fn test_existing_type_preserved() {
        let mut schema = json!({
            "properties": {
                "name": {
                    "type": "string",
                    "anyOf": [
                        {"type": "number"}
                    ]
                }
            }
        });

        clean_json_schema(&mut schema);

        // type 已存在，不应被 anyOf 中的类型覆盖
        assert_eq!(schema["properties"]["name"]["type"], "string");
        assert!(schema["properties"]["name"].get("anyOf").is_none());
    }

    // [NEW TEST] 验证 Issue #815: anyOf 内部属性不丢失
    #[test]
    fn test_issue_815_anyof_properties_preserved() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "config": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "recursive": { "type": "boolean" }
                            },
                            "required": ["path"]
                        },
                        { "type": "null" }
                    ]
                }
            }
        });

        clean_json_schema(&mut schema);

        let config = &schema["properties"]["config"];

        // 1. 验证类型被提取
        assert_eq!(config["type"], "object");

        // 2. 验证 anyOf 内部的 properties 被合并上来了
        assert!(config.get("properties").is_some());
        assert_eq!(config["properties"]["path"]["type"], "string");
        assert_eq!(config["properties"]["recursive"]["type"], "boolean");

        // 3. 验证 required 被合并上来了
        let req = config["required"].as_array().unwrap();
        assert!(req.iter().any(|v| v == "path"));

        // 4. 验证 anyOf 字段本身被移除
        assert!(config.get("anyOf").is_none());

        // 5. 验证没有因为“空”而注入 reason (因为我们保留了属性)
        assert!(config["properties"].get("reason").is_none());
    }

    // [NEW TEST] 验证安全检查：不应处理非 Schema 对象（保护工具调用）
    #[test]
    fn test_clean_json_schema_on_non_schema_object() {
        // 模拟 request.rs 中转换了一半的 functionCall 对象
        let mut tool_call = json!({
            "functionCall": {
                "name": "local_shell_call",
                "args": { "command": ["ls"] },
                "id": "call_123"
            }
        });

        // 调用清洗逻辑
        clean_json_schema(&mut tool_call);

        // 验证：这些非 Schema 字段不应被移除（因为不符合 looks_like_schema 判定）
        let fc = &tool_call["functionCall"];
        assert_eq!(fc["name"], "local_shell_call");
        assert_eq!(fc["args"]["command"][0], "ls");
        assert_eq!(fc["id"], "call_123");
    }

    // [NEW TEST] 验证 Nullable 处理
    #[test]
    fn test_nullable_handling_with_description() {
        let mut schema = json!({
            "type": ["string", "null"],
            "description": "User name"
        });

        clean_json_schema(&mut schema);

        // 验证 type 被降级，且描述被追加 (nullable)
        assert_eq!(schema["type"], "string");
        assert!(
            schema["description"]
                .as_str()
                .unwrap()
                .contains("User name")
        );
        assert!(
            schema["description"]
                .as_str()
                .unwrap()
                .contains("(nullable)")
        );
    }

    // [NEW TEST] 验证 anyOf 内部的 propertyNames 被移除
    #[test]
    fn test_clean_anyof_with_propertynames() {
        let mut schema = json!({
            "properties": {
                "config": {
                    "anyOf": [
                        {
                            "type": "object",
                            "propertyNames": {"pattern": "^[a-z]+$"},
                            "properties": {
                                "key": {"type": "string"}
                            }
                        },
                        {"type": "null"}
                    ]
                }
            }
        });

        clean_json_schema(&mut schema);

        // 验证 anyOf 被移除（已被合并）
        let config = &schema["properties"]["config"];
        assert!(config.get("anyOf").is_none());

        // 验证 propertyNames 被移除
        assert!(config.get("propertyNames").is_none());

        // 验证合并后的 properties 存在且没有 propertyNames
        assert!(config.get("properties").is_some());
        assert_eq!(config["properties"]["key"]["type"], "string");
    }

    // [NEW TEST] 验证 items 数组中的 const 被移除
    #[test]
    fn test_clean_items_array_with_const() {
        let mut schema = json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "status": {
                        "const": "active",
                        "type": "string"
                    }
                }
            }
        });

        clean_json_schema(&mut schema);

        // 验证 const 被移除
        let status = &schema["items"]["properties"]["status"];
        assert!(status.get("const").is_none());

        // 验证 type 仍然存在
        assert_eq!(status["type"], "string");
    }

    // [NEW TEST] 验证多层嵌套数组的清理
    #[test]
    fn test_deep_nested_array_cleaning() {
        let mut schema = json!({
            "properties": {
                "data": {
                    "anyOf": [
                        {
                            "type": "array",
                            "items": {
                                "anyOf": [
                                    {
                                        "type": "object",
                                        "propertyNames": {"maxLength": 10},
                                        "const": "test",
                                        "properties": {
                                            "name": {"type": "string"}
                                        }
                                    },
                                    {"type": "null"}
                                ]
                            }
                        }
                    ]
                }
            }
        });

        clean_json_schema(&mut schema);

        // 验证深层嵌套的非法字段都被移除
        let data = &schema["properties"]["data"];

        // anyOf 应该被合并移除
        assert!(data.get("anyOf").is_none());

        // 验证没有 propertyNames 和 const 逃逸到顶层
        assert!(data.get("propertyNames").is_none());
        assert!(data.get("const").is_none());

        // 验证结构被正确保留
        assert_eq!(data["type"], "array");
        if let Some(items) = data.get("items") {
            // items 内部的 anyOf 也应该被合并
            assert!(items.get("anyOf").is_none());
            assert!(items.get("propertyNames").is_none());
            assert!(items.get("const").is_none());
        }
    }

    #[test]
    fn test_fix_tool_call_args() {
        let mut args = serde_json::json!({
            "port": "8080",
            "enabled": "true",
            "timeout": "5.5",
            "metadata": {
                "retry": "3"
            },
            "tags": ["1", "2"]
        });

        let schema = serde_json::json!({
            "properties": {
                "port": { "type": "integer" },
                "enabled": { "type": "boolean" },
                "timeout": { "type": "number" },
                "metadata": {
                    "type": "object",
                    "properties": {
                        "retry": { "type": "integer" }
                    }
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "integer" }
                }
            }
        });

        fix_tool_call_args(&mut args, &schema);

        assert_eq!(args["port"], 8080);
        assert_eq!(args["enabled"], true);
        assert_eq!(args["timeout"], 5.5);
        assert_eq!(args["metadata"]["retry"], 3);
        assert_eq!(args["tags"], serde_json::json!([1, 2]));
    }

    #[test]
    fn test_fix_tool_call_args_protection() {
        let mut args = serde_json::json!({
            "version": "01.0",
            "code": "007"
        });

        let schema = serde_json::json!({
            "properties": {
                "version": { "type": "number" },
                "code": { "type": "integer" }
            }
        });

        fix_tool_call_args(&mut args, &schema);

        // 应保留字符串以防破坏语义
        assert_eq!(args["version"], "01.0");
        assert_eq!(args["code"], "007");
    }

    // [NEW TEST #952] 验证嵌套层级的 $defs 能被正确收集和展开
    #[test]
    fn test_nested_defs_flattening() {
        // MCP 工具常常将 $defs 嵌套在 properties 内部，而非根层级
        let mut schema = json!({
            "type": "object",
            "properties": {
                "config": {
                    "$defs": {
                        "Address": {
                            "type": "object",
                            "properties": {
                                "city": { "type": "string" },
                                "zip": { "type": "string" }
                            }
                        }
                    },
                    "type": "object",
                    "properties": {
                        "home": { "$ref": "#/$defs/Address" },
                        "work": { "$ref": "#/$defs/Address" }
                    }
                }
            }
        });

        clean_json_schema(&mut schema);

        // 验证嵌套的 $ref 被正确解析
        let home = &schema["properties"]["config"]["properties"]["home"];
        assert_eq!(
            home["type"], "object",
            "home should have type 'object' from resolved $ref"
        );
        assert_eq!(
            home["properties"]["city"]["type"], "string",
            "home.properties.city should exist from resolved Address"
        );

        // 验证没有残留的 $ref
        assert!(
            home.get("$ref").is_none(),
            "home should not have orphan $ref"
        );

        // 验证 work 也被正确解析
        let work = &schema["properties"]["config"]["properties"]["work"];
        assert_eq!(work["type"], "object");
        assert!(work.get("$ref").is_none());
    }

    // [NEW TEST #952] 验证无法解析的 $ref 被优雅降级
    #[test]
    fn test_unresolved_ref_fallback() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "external": { "$ref": "https://example.com/schemas/External.json" },
                "missing": { "$ref": "#/$defs/NonExistent" }
            }
        });

        clean_json_schema(&mut schema);

        // 验证外部引用被降级为 string 类型
        let external = &schema["properties"]["external"];
        assert_eq!(
            external["type"], "string",
            "unresolved external $ref should fallback to string"
        );
        assert!(
            external["description"]
                .as_str()
                .unwrap()
                .contains("Unresolved $ref"),
            "description should contain unresolved $ref hint"
        );

        // 验证内部缺失引用也被降级
        let missing = &schema["properties"]["missing"];
        assert_eq!(missing["type"], "string");
        assert!(
            missing["description"]
                .as_str()
                .unwrap()
                .contains("NonExistent")
        );
    }

    // [NEW TEST #952] 验证深层嵌套的多级 $defs 都能被收集
    #[test]
    fn test_deeply_nested_multi_level_defs() {
        let mut schema = json!({
            "type": "object",
            "$defs": {
                "RootDef": { "type": "integer" }
            },
            "properties": {
                "level1": {
                    "type": "object",
                    "$defs": {
                        "Level1Def": { "type": "boolean" }
                    },
                    "properties": {
                        "level2": {
                            "type": "object",
                            "$defs": {
                                "Level2Def": { "type": "number" }
                            },
                            "properties": {
                                "useRoot": { "$ref": "#/$defs/RootDef" },
                                "useLevel1": { "$ref": "#/$defs/Level1Def" },
                                "useLevel2": { "$ref": "#/$defs/Level2Def" }
                            }
                        }
                    }
                }
            }
        });

        clean_json_schema(&mut schema);

        let level2_props = &schema["properties"]["level1"]["properties"]["level2"]["properties"];

        // 验证所有层级的 $defs 都被正确解析
        assert_eq!(
            level2_props["useRoot"]["type"], "integer",
            "RootDef should resolve"
        );
        assert_eq!(
            level2_props["useLevel1"]["type"], "boolean",
            "Level1Def should resolve"
        );
        assert_eq!(
            level2_props["useLevel2"]["type"], "number",
            "Level2Def should resolve"
        );

        // 验证没有残留 $ref
        assert!(level2_props["useRoot"].get("$ref").is_none());
        assert!(level2_props["useLevel1"].get("$ref").is_none());
        assert!(level2_props["useLevel2"].get("$ref").is_none());
    }

    // [NEW TEST] 验证对非标准字段（如 cornerRadius）的清洗和启发式修复
    #[test]
    fn test_non_standard_field_cleaning_and_healing() {
        let mut schema = json!({
            "type": "array",
            "items": {
                "cornerRadius": { "type": "number" },
                "fillColor": { "type": "string" }
            }
        });

        clean_json_schema(&mut schema);

        // 验证 items 中的非标准字段被移动到了 properties 内部，并增加了 type: object
        let items = &schema["items"];
        assert_eq!(
            items["type"], "object",
            "Malformed items should be healed to type object"
        );
        assert!(
            items.get("properties").is_some(),
            "Malformed items should have properties object"
        );
        assert_eq!(items["properties"]["cornerRadius"]["type"], "number");
        assert_eq!(items["properties"]["fillColor"]["type"], "string");

        // 验证原始字段已从 items 顶层移除（白名单过滤）
        assert!(items.get("cornerRadius").is_none());
        assert!(items.get("fillColor").is_none());
    }

    // [NEW TEST] 验证隐式 Array (只有 items) 和隐式 Object (只有 properties) 的处理
    #[test]
    fn test_implicit_type_injection() {
        let mut schema = json!({
            "properties": {
                "values": {
                    "items": {
                        "cornerRadius": { "type": "number" }
                    }
                }
            }
        });

        clean_json_schema(&mut schema);

        // 验证 values 被注入了 type: array
        assert_eq!(schema["properties"]["values"]["type"], "array");

        // 验证 items 被启发式修复为 type: object 并包含 properties
        let items = &schema["properties"]["values"]["items"];
        assert_eq!(items["type"], "object");
        assert!(items["properties"].get("cornerRadius").is_some());
    }

    #[test]
    fn test_gemini_strict_validation_injection() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "patterns": {
                    "items": {
                        "properties": {
                            "type": {
                                "enum": ["A", "B"]
                            }
                        }
                    }
                },
                "nested_props": {
                    "properties": {
                        "foo": { "type": "string" }
                    }
                }
            }
        });

        clean_json_schema(&mut schema);

        // 验证 enum 自动补全了 type: string
        let type_node = &schema["properties"]["patterns"]["items"]["properties"]["type"];
        assert_eq!(type_node["type"], "string");
        assert!(type_node.get("enum").is_some());

        // 验证 嵌套 properties 自动补全了 type: object
        assert_eq!(schema["properties"]["nested_props"]["type"], "object");

        // 验证 patterns 自动补全了 type: array
        assert_eq!(schema["properties"]["patterns"]["type"], "array");
    }
    #[test]
    fn test_malformed_items_as_properties() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "config": {
                    "type": "object",
                    "items": {
                        "color": { "type": "string" },
                        "size": { "type": "number" }
                    }
                }
            }
        });

        clean_json_schema(&mut schema);

        // 验证 items 被移除并转换为 properties
        let config = &schema["properties"]["config"];
        assert!(config.get("items").is_none());
        assert_eq!(config["properties"]["color"]["type"], "string");
        assert_eq!(config["properties"]["size"]["type"], "number");
        assert_eq!(config["type"], "object");
    }

    #[test]
    fn test_merge_all_of() {
        let mut schema = json!({
            "allOf": [
                {
                    "type": "object",
                    "properties": {
                        "base_prop": { "type": "string" }
                    },
                    "required": ["base_prop"]
                },
                {
                    "type": "object",
                    "properties": {
                        "extended_prop": { "type": "number" }
                    },
                    "required": ["extended_prop"]
                }
            ]
        });

        clean_json_schema(&mut schema);

        assert!(schema.get("allOf").is_none());
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].get("base_prop").is_some());
        assert_eq!(schema["properties"]["base_prop"]["type"], "string");
        assert!(schema["properties"].get("extended_prop").is_some());
        assert_eq!(schema["properties"]["extended_prop"]["type"], "number");

        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "base_prop"));
        assert!(required.iter().any(|v| v == "extended_prop"));
    }

    #[test]
    fn test_circular_ref_flattening() {
        // 模拟循环引用：A -> B, B -> A
        let mut schema = json!({
            "$defs": {
                "A": {
                    "type": "object",
                    "properties": {
                        "toB": { "$ref": "#/$defs/B" }
                    }
                },
                "B": {
                    "type": "object",
                    "properties": {
                        "toA": { "$ref": "#/$defs/A" }
                    }
                }
            },
            "properties": {
                "start": { "$ref": "#/$defs/A" }
            }
        });

        // 如果没有深度限制，这里会发生栈溢出
        // 有了深度限制，它应该能正常返回（尽管展开是不完整的）
        clean_json_schema(&mut schema);

        // 验证基本结构保留，没有崩溃
        assert_eq!(schema["properties"]["start"]["type"], "object");
        assert!(
            schema["properties"]["start"]["properties"]
                .get("toB")
                .is_some()
        );
    }

    #[test]
    fn test_any_of_best_branch_selection() {
        let mut schema = json!({
            "anyOf": [
                { "type": "string" },
                { "type": "object", "properties": { "foo": { "type": "string" } } },
                { "type": "null" }
            ]
        });

        clean_json_schema(&mut schema);

        // 验证选择了分数最高的 Object 分支
        assert_eq!(schema["type"], "object");
        assert!(schema.get("properties").is_some());
        assert_eq!(schema["properties"]["foo"]["type"], "string");

        // 验证描述中增加了类型提示 (注意: null 分支在清洗后变为了带 (nullable) 标记的 string，因此去重后为 string | object)
        assert!(
            schema["description"]
                .as_str()
                .unwrap()
                .contains("Accepts: string | object")
        );
    }

    #[test]
    fn test_coerce_helpers() {
        assert_eq!(coerce_str_to_boolean("true"), Some(true));
        assert_eq!(coerce_str_to_boolean("1"), Some(true));
        assert_eq!(coerce_str_to_boolean("YES"), Some(true));
        assert_eq!(coerce_str_to_boolean("on"), Some(true));

        assert_eq!(coerce_str_to_boolean("false"), Some(false));
        assert_eq!(coerce_str_to_boolean("0"), Some(false));
        assert_eq!(coerce_str_to_boolean("NO"), Some(false));
        assert_eq!(coerce_str_to_boolean("off"), Some(false));

        assert_eq!(coerce_str_to_boolean("invalid"), None);

        assert!(is_preserved_string_number("01"));
        assert!(is_preserved_string_number("007"));
        assert!(!is_preserved_string_number("0.5"));
        assert!(!is_preserved_string_number("123"));
    }
}
