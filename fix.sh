#!/bin/bash
sed -i 's/Ipv4Addr::new(\n            127, 0, 0, 1\n        )/Ipv4Addr::LOCALHOST/g' crates/openproxy-adapters/src/upstream/connector.rs
sed -i 's/Ipv4Addr::new(\n            0, 0, 0, 0\n        )/Ipv4Addr::UNSPECIFIED/g' crates/openproxy-adapters/src/upstream/connector.rs
sed -i 's/Ipv6Addr::new(\n            0, 0, 0, 0, 0, 0, 0, 1\n        )/Ipv6Addr::LOCALHOST/g' crates/openproxy-adapters/src/upstream/connector.rs
