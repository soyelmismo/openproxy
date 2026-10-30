use std::time::Instant;
fn main() {
    let pool = openproxy_db::DbPool::open(std::path::Path::new("/root/.openproxy/data.db"))
        .unwrap_or_else(|e| {
            eprintln!("open pool: {e}");
            std::process::exit(1);
        });
    let conn = pool.reader();
    // Warm
    for _ in 0..10 {
        let _ =
            openproxy_core::routing::resolve_with_options(&conn, "devin/swe-2:free", None, None);
        let _ = openproxy_core::routing::snapshot::resolve_with_options_snap(
            &pool,
            "devin/swe-2:free",
            None,
            None,
        );
    }
    let n = 2000u32;
    let t = Instant::now();
    for _ in 0..n {
        let _ = std::hint::black_box(openproxy_core::routing::resolve_with_options(
            &conn,
            "devin/swe-2:free",
            None,
            None,
        ));
    }
    let sql_ns = t.elapsed().as_nanos() as f64 / n as f64;
    let t = Instant::now();
    for _ in 0..n {
        let _ = std::hint::black_box(
            openproxy_core::routing::snapshot::resolve_with_options_snap(
                &pool,
                "devin/swe-2:free",
                None,
                None,
            ),
        );
    }
    let snap_ns = t.elapsed().as_nanos() as f64 / n as f64;
    // combo path
    let combo = "combo:cheapest";
    let t = Instant::now();
    for _ in 0..n {
        let _ = std::hint::black_box(openproxy_core::routing::resolve_with_options(
            &conn, combo, None, None,
        ));
    }
    let sql_c = t.elapsed().as_nanos() as f64 / n as f64;
    let t = Instant::now();
    for _ in 0..n {
        let _ = std::hint::black_box(
            openproxy_core::routing::snapshot::resolve_with_options_snap(&pool, combo, None, None),
        );
    }
    let snap_c = t.elapsed().as_nanos() as f64 / n as f64;
    println!(
        "direct  sql={sql_ns:.0}ns snap={snap_ns:.0}ns speedup={:.1}x",
        sql_ns / snap_ns
    );
    println!(
        "combo   sql={sql_c:.0}ns snap={snap_c:.0}ns speedup={:.1}x",
        sql_c / snap_c
    );
}
