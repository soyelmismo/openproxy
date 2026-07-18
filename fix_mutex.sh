sed -i 's/let w = db_pool.writer();/let w = db_pool.writer();\n                drop(w);/g' crates/openproxy-core/src/discovery_scheduler.rs
