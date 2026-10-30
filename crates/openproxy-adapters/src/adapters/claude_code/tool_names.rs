/// Builds the forward rename map (original -> cloaked) for a set of tool names.
///
/// A name already in TitleCase is identity and stays out of the map. Two names
/// that collide on the same cloaked form are both left unchanged: renaming them
/// would make them indistinguishable to the upstream.
pub fn build_tool_rename_map_from_names<'a, I>(
    names: I,
) -> std::collections::HashMap<String, String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut candidates: Vec<(String, String)> = Vec::new();
    for name in names {
        let cloaked = title_case_tool_name(name);
        if cloaked.is_empty() || cloaked == name {
            continue;
        }
        if candidates.iter().any(|(original, _)| original == name) {
            continue;
        }
        candidates.push((name.to_string(), cloaked));
    }

    let mut map = std::collections::HashMap::with_capacity(candidates.len());
    for (original, cloaked) in &candidates {
        let collides = candidates.iter().any(|(other_original, other_cloaked)| {
            other_cloaked == cloaked && other_original != original
        });
        if !collides {
            map.insert(original.clone(), cloaked.clone());
        }
    }
    map
}

/// Splits a tool name into TitleCase segments: `browser_exec` becomes
/// `BrowserExec`, `read-file` becomes `ReadFile`. Segments split on `_` and `-`
/// and keep their inner casing, so an already-TitleCase name maps to itself.
pub fn title_case_tool_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for segment in name.split(['_', '-']) {
        let mut chars = segment.chars();
        let Some(first) = chars.next() else {
            continue;
        };
        out.extend(first.to_uppercase());
        out.push_str(chars.as_str());
    }
    out
}

/// Builds the rename map from the `tools` declared in an Anthropic request body.
pub fn build_tool_rename_map(
    json: &serde_json::Value,
) -> std::collections::HashMap<String, String> {
    let Some(tools) = json.get("tools").and_then(|t| t.as_array()) else {
        return std::collections::HashMap::new();
    };
    build_tool_rename_map_from_names(
        tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str())),
    )
}

/// Renames the tool names of an Anthropic request body in place: the declared
/// `tools`, any `tool_use` blocks in the conversation history, and a pinned
/// `tool_choice`. `tool_result` blocks reference tools by id, so they carry no
/// name to rewrite.
pub fn rename_tool_names_in_body(
    json: &mut serde_json::Value,
    map: &std::collections::HashMap<String, String>,
) {
    if map.is_empty() {
        return;
    }
    if let Some(tools) = json.get_mut("tools").and_then(|t| t.as_array_mut()) {
        for tool in tools {
            rename_name_field(tool, map);
        }
    }
    if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for message in messages {
            let Some(content) = message.get_mut("content").and_then(|c| c.as_array_mut()) else {
                continue;
            };
            for block in content {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    rename_name_field(block, map);
                }
            }
        }
    }
    if let Some(tool_choice) = json.get_mut("tool_choice") {
        rename_name_field(tool_choice, map);
    }
}

fn rename_name_field(
    value: &mut serde_json::Value,
    map: &std::collections::HashMap<String, String>,
) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let Some(name) = obj.get("name").and_then(|n| n.as_str()) else {
        return;
    };
    let Some(renamed) = map.get(name) else {
        return;
    };
    obj.insert(
        "name".to_string(),
        serde_json::Value::String(renamed.clone()),
    );
}
