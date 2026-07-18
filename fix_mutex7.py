import re

with open("crates/openproxy-core/src/discovery_scheduler.rs", "r") as f:
    content = f.read()

pattern = r'let w = db_pool\.writer\(\);\s+match accounts::decrypt_api_key\(&w, acc\.id, master_key\.as_ref\(\)\) \{'
replacement = r'''let key_res = {
                    let w = db_pool.writer();
                    accounts::decrypt_api_key(&w, acc.id, master_key.as_ref())
                };
                match key_res {'''

new_content = re.sub(pattern, replacement, content)

with open("crates/openproxy-core/src/discovery_scheduler.rs", "w") as f:
    f.write(new_content)
