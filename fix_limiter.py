import re

with open("crates/openproxy-core/src/oauth/mod.rs", "r") as f:
    content = f.read()

# Replace the while loop with nothing.
content = content.replace("        // Consume the initial token so we don't burst the first request.\n        while limiter.check().is_ok() {}\n", "")

with open("crates/openproxy-core/src/oauth/mod.rs", "w") as f:
    f.write(content)

print("Rewritten")
