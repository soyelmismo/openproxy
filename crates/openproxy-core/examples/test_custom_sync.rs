use openproxy_core::free_proxies::{fetch_custom_proxy_source, upsert_scraped_proxies};
use openproxy_db::DbPool;

#[tokio::main]
async fn main() {
    let url = "https://proxy.webshare.io/api/v2/proxy/list/download/lrudumdwkaeblzdpofyztfcfzcqeirkioveeirws/-/any/username/direct/-/?plan_id=13938906";
    let name = "bigdataligma@gmail.com";

    println!("Fetching from: {url}");
    match fetch_custom_proxy_source(name, url, 0).await {
        Ok(list) => {
            println!("Fetched {} proxies", list.len());
            for p in list.iter().take(2) {
                println!("{p:?}");
            }

            // Try upsert
            let pool = DbPool::open(std::path::Path::new("/root/.openproxy/data.db")).unwrap();
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
