//! Throwaway diagnostic: export each zai account's stored credential to the
//! environment for a manual curl probe. The credential is NOT printed; the
//! shell reads it from a pipe so curl can use it.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mk = openproxy_db::secrets::MasterKey::from_env()?;
    let conn = rusqlite::Connection::open("file:/root/.openproxy/data.db?mode=ro")?;
    let aid: i64 = std::env::args()
        .nth(1)
        .ok_or("usage: zai_probe <account_id>")?
        .parse()
        .map_err(|_| "account id must be an integer")?;
    let raw: Option<String> = conn
        .query_row(
            "SELECT oauth_provider_specific FROM accounts WHERE id=?1",
            [aid],
            |r| r.get(0),
        )
        .map_err(|e| format!("account {aid}: {e}"))?;
    let Some(json) = openproxy_db::accounts::decrypt_oauth_provider_specific(raw.as_deref(), &mk)
    else {
        eprintln!("no oauth_provider_specific for {aid}");
        std::process::exit(2);
    };
    let v: serde_json::Value = serde_json::from_str(&json)?;
    // Write to stdout: the caller pipes this straight into curl -H.
    print!(
        "{}",
        v.get("autoclaw_access_token")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
    );
    Ok(())
}
