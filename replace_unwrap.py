import os
import re

files_to_check = [
    "crates/openproxy-server/src/handlers/admin/combos.rs",
    "crates/openproxy-server/src/handlers/admin/oauth.rs",
]

pattern = re.compile(r'\.await\s*\.unwrap\(\)')

for filepath in files_to_check:
    with open(filepath, 'r') as f:
        content = f.read()

    new_content = pattern.sub(r'.await.map_err(|e| ApiError(CoreError::Internal(format!("spawn_blocking failed: {}", e))))?', content)

    with open(filepath, 'w') as f:
        f.write(new_content)
