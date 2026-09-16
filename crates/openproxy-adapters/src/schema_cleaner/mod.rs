mod defs;
mod objects;
pub(crate) mod sanitize;
#[cfg(test)]
mod tests;

pub use defs::MAX_RECURSION_DEPTH;
pub use sanitize::fix_tool_call_args;

use serde_json::Value;

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
    defs::collect_all_defs(value, &mut all_defs);

    // 移除根层级的 $defs/definitions (保持向后兼容)
    if let Value::Object(map) = value {
        map.remove("$defs");
        map.remove("definitions");
    }

    // [FIX #952] 始终运行 flatten_refs，即使 defs 为空
    // 这样可以捕获并处理无法解析的 $ref (降级为 string 类型)
    if let Value::Object(map) = value {
        defs::flatten_refs(map, &all_defs, 0);
    }

    // 递归清理
    clean_json_schema_recursive(value, true, 0);
}

pub(crate) fn clean_json_schema_recursive(
    value: &mut Value,
    is_schema_node: bool,
    depth: usize,
) -> bool {
    if depth > MAX_RECURSION_DEPTH {
        debug_assert!(
            false,
            "Max recursion depth reached in clean_json_schema_recursive"
        );
        return false;
    }

    match value {
        Value::Object(map) => objects::clean_object_schema(map, is_schema_node, depth),
        Value::Array(arr) => {
            objects::clean_array_schema(arr, is_schema_node, depth);
            false
        }
        _ => false,
    }
}
