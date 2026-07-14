use regex::Regex;
use once_cell::sync::Lazy;
static RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[a-z]{2}-[a-z]+-[0-9]").unwrap());
fn main() {
    println!("{}", RE.is_match("us-east-1"));
}
