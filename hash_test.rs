use openproxy_core::api_keys::hash_key;
fn main() {
    let hash = hash_key("op_live_test_dummy_token_for_e2e");
    println!("HASH: {}", hash);
}
