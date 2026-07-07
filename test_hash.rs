use sha2::{Digest, Sha256};
fn main() {
    let plaintext = "op_live_test_dummy_token_for_e2e";
    let mut hasher = Sha256::new();
    hasher.update(plaintext.as_bytes());
    let h = hex::encode(hasher.finalize());
    println!("{}", h);
}
