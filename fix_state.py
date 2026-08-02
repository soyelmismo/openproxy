with open("crates/openproxy-server/src/state.rs", "r") as f:
    content = f.read()

content = content.replace("let adapters_snapshot = adapters.read().clone();", "let adapters_snapshot = Arc::clone(&adapters.read());")
content = content.replace("self.adapters.read().clone()", "Arc::clone(&self.adapters.read())")
content = content.replace("let adapters_clone = adapters.read().clone();", "let adapters_clone = Arc::clone(&adapters.read());")

with open("crates/openproxy-server/src/state.rs", "w") as f:
    f.write(content)
