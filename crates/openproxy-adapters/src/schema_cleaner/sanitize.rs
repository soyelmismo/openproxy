use serde_json::Value;

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

fn clean_union_branches(map: &mut serde_json::Map<String, Value>, depth: usize) {
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(arr)) = map.get_mut(key) {
            for branch in arr.iter_mut() {
                super::clean_json_schema_recursive(branch, true, depth + 1);
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

pub fn clean_unions_and_hints(map: &mut serde_json::Map<String, Value>, depth: usize) {
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
            super::clean_json_schema_recursive(v, true, depth + 1);
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

pub fn sanitize_schema_fields(
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

fn extract_best_schema_from_union(union_array: &[Value]) -> Option<(Value, Vec<String>)> {
    let mut best_option: Option<&Value> = None;
    let mut best_score = -1;
    let mut all_types = Vec::new();

    for item in union_array {
        let score = score_schema_option(item);

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

pub(crate) fn is_preserved_string_number(s: &str) -> bool {
    s.starts_with('0') && s.len() > 1 && !s.starts_with("0.")
}

fn coerce_to_number(value: &mut Value) {
    let Some(s) = value.as_str() else {
        return;
    };
    if is_preserved_string_number(s) {
        return;
    }

    if let Ok(i) = s.parse::<i64>() {
        *value = Value::Number(serde_json::Number::from(i));
    } else if let Some(n) = s.parse::<f64>().ok().and_then(serde_json::Number::from_f64) {
        *value = Value::Number(n);
    }
}

pub(crate) fn coerce_str_to_boolean(s: &str) -> Option<bool> {
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

fn fix_single_arg_recursive(value: &mut Value, schema: &Value) {
    if let Some(nested_props) = schema.get("properties").and_then(|p| p.as_object()) {
        fix_object_arg(value, nested_props);
        return;
    }

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

    fix_scalar_arg(value, schema_type.as_str());
}
