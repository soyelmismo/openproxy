#![allow(clippy::unwrap_used)]

use openproxy_core::free_proxies::{fetch_custom_proxy_source, upsert_scraped_proxies};
use openproxy_db::DbPool;

#[tokio::main]
async fn main() {
    let url = std::env::var("PROXY_SOURCE_URL")
        .unwrap_or_else(|_| "https://example.com/mock-proxy-list.txt".to_string());
    let name = "example-source";

    println!("Fetching from: {url}");
    match fetch_custom_proxy_source(name, &url, 0).await {
        Ok(list) => {
            println!("Fetched {} proxies", list.len());
            for p in list.iter().take(2) {
                println!("{p:?}");
            }

            // Try upsert
            let db_path = std::env::var("OPENPROXY_DATABASE_PATH")
                .unwrap_or_else(|_| "./target/test-data.db".to_string());
            let pool = DbPool::open(std::path::Path::new(&db_path)).unwrap();
            let mut conn = pool.writer();
            match upsert_scraped_proxies(&mut conn, &list) {
                Ok(()) => println!("Upsert successful"),
                Err(e) => println!("Upsert failed: {e:?}"),
            }

            // Check count
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM free_proxies WHERE source = ?1",
                    rusqlite::params![name],
                    |r| r.get(0),
                )
                .unwrap();
            println!("Count in DB for {name}: {count}");
        }
        Err(e) => {
            println!("Error: {e:?}");
        }
    }
}
