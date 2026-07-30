import os
import re

files_to_check = [
    "crates/openproxy-server/src/handlers/admin/auth.rs",
    "crates/openproxy-server/src/handlers/models.rs"
]

pattern = re.compile(r'tokio::task::spawn_blocking\(.*?\)\s*\.unwrap\(\)', re.DOTALL)
pattern_unwrap = re.compile(r'\.unwrap\(\)')

for filepath in files_to_check:
    with open(filepath, 'r') as f:
        content = f.read()

    new_content = pattern_unwrap.sub(r'.unwrap_or_else(|e| Err(ApiError(CoreError::Internal(format!("spawn_blocking failed: {}", e)))))', content)
    # Just to verify there are no unwraps on spawn_blocking
    # Actually wait, auth.rs and models.rs do NOT await spawn_blocking, they just spawn it in the background!
    # They do not return a result, so there is no unwrap.
    # Let me check auth.rs and models.rs.
    pass
