import re

with open("crates/openproxy-core/benches/oauth_refresh.rs", "r") as f:
    content = f.read()

# Replace the while loop with nothing.
content = content.replace("            // consume initial burst\n            while limiter.check().is_ok() {}\n", "")

with open("crates/openproxy-core/benches/oauth_refresh.rs", "w") as f:
    f.write(content)

print("Rewritten bench")
