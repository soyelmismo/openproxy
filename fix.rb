content1 = File.read("crates/openproxy-adapters/src/adapters/kiro_ai.rs")
content1.gsub!('.expect("valid regex")', '.expect("invalid filter pattern")')
File.write("crates/openproxy-adapters/src/adapters/kiro_ai.rs", content1)

content2 = File.read("crates/openproxy-compression/src/rtk/line_filter.rs")
content2.gsub!('.unwrap_or_else(|e| panic!("invalid filter pattern {pattern:?}: {e}"))', '.expect("invalid filter pattern")')
content2.gsub!('.expect("valid regex")', '.expect("invalid filter pattern")')
File.write("crates/openproxy-compression/src/rtk/line_filter.rs", content2)

content3 = File.read("crates/openproxy-compression/src/content_router.rs")
content3.gsub!('.expect("valid regex")', '.expect("invalid filter pattern")')
File.write("crates/openproxy-compression/src/content_router.rs", content3)
