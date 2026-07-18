cat crates/openproxy-core/src/discovery_scheduler.rs | awk '
BEGIN { in_target = 0 }
/let w = db_pool.writer();/ {
    if (in_target == 0) {
        print "                let key_res = {"
        print "                    let w = db_pool.writer();"
        print "                    accounts::decrypt_api_key(&w, acc.id, master_key.as_ref())"
        print "                };"
        print "                match key_res {"
        in_target = 1
        next
    }
}
{ print }
' > tmp.rs && mv tmp.rs crates/openproxy-core/src/discovery_scheduler.rs
