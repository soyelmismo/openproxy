cat crates/openproxy-pipeline/src/predictive_rate_limit.rs | \
awk '
    /let mut limit = body.len\(\).min\(64\);/ {
        print "            let limit = body.floor_char_boundary(64.min(body.len()));\n            let prefix = &body[..limit];"
        skip=4
        next
    }
    /let mut limit = msg.len\(\).min\(64\);/ {
        print "            let limit = msg.floor_char_boundary(64.min(msg.len()));\n            let prefix = &msg[..limit];"
        skip=4
        next
    }
    skip > 0 {
        skip--
        next
    }
    {print}
' > crates/openproxy-pipeline/src/predictive_rate_limit.rs.tmp
mv crates/openproxy-pipeline/src/predictive_rate_limit.rs.tmp crates/openproxy-pipeline/src/predictive_rate_limit.rs
