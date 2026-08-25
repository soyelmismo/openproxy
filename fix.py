import re

def fix_file(filepath):
    with open(filepath, 'r') as f:
        content = f.read()

    replacement = """        .and_then(|s| {
            use subtle::ConstantTimeEq;
            let b = s.as_bytes();
            if b.len() >= 7 && bool::from(b[..7].ct_eq(b"Bearer ")) {
                Some(s[7..].trim())
            } else {
                None
            }
        })"""

    content = re.sub(
        r'\.and_then\(\|s\| s\.strip_prefix\("Bearer "\)\)\n\s*\.map\(str::trim\)',
        replacement,
        content
    )

    with open(filepath, 'w') as f:
        f.write(content)

fix_file('crates/openproxy-server/src/middleware/auth.rs')
fix_file('crates/openproxy-server/src/handlers/admin/auth.rs')
