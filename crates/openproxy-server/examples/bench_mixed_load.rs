//! Manual isolated mixed-load benchmark. Never run by tests or CI.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
#[path = "bench_mixed_load/config.rs"]
mod config;
#[path = "bench_mixed_load/load.rs"]
mod load;
#[path = "bench_mixed_load/world.rs"]
mod world;
use config::{Config, EndpointStats, PeakGauge, Sample, percentile};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

fn merge(a: &mut Sample, b: Sample) {
    a.ok += b.ok;
    a.failed += b.failed;
    a.lat_us.extend(b.lat_us);
    for (status, n) in b.statuses {
        if let Some(e) = a.statuses.iter_mut().find(|e| e.0 == status) {
            e.1 += n;
        } else {
            a.statuses.push((status, n));
        }
    }
}
fn samples() -> load::PurchaseHandles {
    load::PurchaseHandles {
        order: Sample::default(),
        status_poll: Sample::default(),
        reveal: Sample::default(),
        ack: Sample::default(),
    }
}
fn targets() -> load::ReadTargets {
    load::ReadTargets {
        models: Sample::default(),
        plan: Sample::default(),
        plans: Sample::default(),
        summary: Sample::default(),
        by_model: Sample::default(),
        by_day: Sample::default(),
        quota: Sample::default(),
    }
}
async fn purchase(
    client: load::HttpClient,
    base: String,
    chain: Arc<world::MockChain>,
) -> (bool, load::PurchaseHandles, Option<String>) {
    let mut s = samples();
    let receipt = uuid::Uuid::new_v4().to_string();
    match load::create_order(&client, &base, 1, receipt.clone(), &chain, &mut s.order).await {
        Ok((code, _)) => {
            if load::wait_minted(
                &client,
                &base,
                &code,
                Instant::now() + Duration::from_secs(120),
                &mut s.status_poll,
            )
            .await
            {
                let key = load::reveal_and_ack(
                    &client,
                    &base,
                    &code,
                    &receipt,
                    &mut s.reveal,
                    &mut s.ack,
                )
                .await;
                (key.is_some(), s, key)
            } else {
                (false, s, None)
            }
        }
        Err(e) => {
            eprintln!("purchase failed: {e}");
            (false, s, None)
        }
    }
}
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.iter().any(|s| s == "--help") {
        println!("{}", config::HELP);
        return Ok(());
    }
    let cfg = Config::parse(&args)?;
    anyhow::ensure!(
        cfg.workers > 0 && cfg.keys > 0,
        "workers and keys must be positive"
    );
    anyhow::ensure!(
        cfg.per_key_limit == 10 && cfg.global_limit == 512,
        "production concurrency caps must remain 10/512"
    );
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cfg.workers)
        .enable_all()
        .build()?
        .block_on(run(cfg))
}
async fn run(cfg: Config) -> anyhow::Result<()> {
    let upstream = world::spawn_paced_upstream(&cfg);
    let (rpc, chain) = world::spawn_mock_chain();
    let w = world::build_world(&cfg, &upstream).await?;
    let base = format!("http://{}", world::serve_router(w.state.clone()).await?);
    eprintln!("isolated router={base}, mock upstream={upstream}, mock rpc={rpc}");
    let mut credits = w.state.config().credits.clone().unwrap();
    credits.rpc_url = rpc;
    let service = openproxy_server::background::credits_watcher::CreditsWatcherService {
        db_pool: w.db_pool.clone(),
        config: credits,
        master_key: w.master_key.clone(),
        upstream_client: Arc::clone(w.state.upstream_client()),
    };
    service.tick().await.map_err(anyhow::Error::msg)?;
    let stop = Arc::new(AtomicBool::new(false));
    let watcher_stop = stop.clone();
    let watcher = tokio::spawn(async move {
        let mut errors = Vec::new();
        let mut credited = 0;
        while !watcher_stop.load(Ordering::Relaxed) {
            match service.tick().await {
                Ok(r) => credited += r.credited,
                Err(e) => {
                    eprintln!("watcher: {e}");
                    errors.push(e);
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        (credited, errors)
    });
    // Existing customers really bought and received their keys before the burst.
    let client = load::make_client();
    let mut user_keys = Vec::new();
    for _ in 0..16 {
        let (ok, _, key) = purchase(client.clone(), base.clone(), chain.clone()).await;
        anyhow::ensure!(ok, "purchased-user setup failed");
        user_keys.push(key.unwrap());
    }
    let hashes: Vec<_> = user_keys
        .iter()
        .map(|k| openproxy_core::api_keys::hash_key(k))
        .collect();
    w.db_pool.spawn_write(move |db| {
        for (i, hash) in hashes.iter().enumerate() {
            db.execute("UPDATE usage SET api_key_id=(SELECT id FROM api_keys WHERE key_hash=?1) WHERE rowid % 16=?2", rusqlite::params![hash, i as i64]).map_err(openproxy_db::error::map_db_error)?;
        }
        Ok(())
    }).await?;
    let user_keys = Arc::new(user_keys);
    let barrier = Arc::new(tokio::sync::Barrier::new(
        cfg.streams + cfg.orders + cfg.reads + 1,
    ));
    let peak = Arc::new(PeakGauge::default());
    let heartbeat = Arc::new(load::Heartbeat::default());
    let hb_stop = Arc::new(AtomicBool::new(false));
    let hb = {
        let h = heartbeat.clone();
        let s = hb_stop.clone();
        tokio::spawn(async move { h.run(s).await })
    };
    let mut streams = Vec::new();
    let mut orders = Vec::new();
    let mut reads = Vec::new();
    for i in 0..cfg.streams {
        let (c, b, k, m, g, p) = (
            client.clone(),
            base.clone(),
            w.stream_keys[i % w.stream_keys.len()].clone(),
            w.model.clone(),
            barrier.clone(),
            peak.clone(),
        );
        streams.push(tokio::spawn(async move {
            g.wait().await;
            load::one_stream(&c, &b, &k, &m, &p).await
        }));
    }
    for _ in 0..cfg.orders {
        let (c, b, ch, g) = (client.clone(), base.clone(), chain.clone(), barrier.clone());
        orders.push(tokio::spawn(async move {
            g.wait().await;
            purchase(c, b, ch).await
        }));
    }
    let read_cap = Arc::new(tokio::sync::Semaphore::new(if cfg.read_concurrency == 0 {
        cfg.reads.max(1)
    } else {
        cfg.read_concurrency
    }));
    for i in 0..cfg.reads {
        let (c, b, ks, g, cap) = (
            client.clone(),
            base.clone(),
            user_keys.clone(),
            barrier.clone(),
            read_cap.clone(),
        );
        reads.push(tokio::spawn(async move {
            g.wait().await;
            let _p = cap.acquire().await.unwrap();
            let mut t = targets();
            load::one_read(&c, &b, &ks[i % ks.len()], i, &mut t).await;
            t
        }));
    }
    let profiling = std::env::var_os("MIX_SQL_PROFILE").is_some();
    if profiling {
        openproxy_db::sql_profile::start();
    }
    let started = Instant::now();
    barrier.wait().await;
    let mut stream_results = Vec::new();
    for h in streams {
        stream_results.push(h.await?);
    }
    let mut funnel = samples();
    let mut complete = 0;
    let mut delivered = std::collections::HashSet::new();
    for h in orders {
        let (ok, s, k) = h.await?;
        complete += usize::from(ok);
        if let Some(k) = k {
            delivered.insert(k);
        }
        merge(&mut funnel.order, s.order);
        merge(&mut funnel.status_poll, s.status_poll);
        merge(&mut funnel.reveal, s.reveal);
        merge(&mut funnel.ack, s.ack);
    }
    let mut t = targets();
    for h in reads {
        let r = h.await?;
        merge(&mut t.models, r.models);
        merge(&mut t.plan, r.plan);
        merge(&mut t.plans, r.plans);
        merge(&mut t.summary, r.summary);
        merge(&mut t.by_model, r.by_model);
        merge(&mut t.by_day, r.by_day);
        merge(&mut t.quota, r.quota);
    }
    let elapsed = started.elapsed();
    hb_stop.store(true, Ordering::Relaxed);
    hb.await?;
    stop.store(true, Ordering::Relaxed);
    let (credited, watcher_errors) = watcher.await?;
    let audit = w
        .db_pool
        .spawn_write(|db| openproxy_db::credits_ledger::reconcile(db))
        .await?;
    let delivery=w.db_pool.spawn_read(|db| {db.query_row("SELECT COUNT(*), COALESCE(SUM(key_revealed),0), SUM(minted_key_encrypted IS NOT NULL) FROM credit_orders",[],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?))).map_err(openproxy_db::error::map_db_error)}).await?;
    let endpoints = vec![
        EndpointStats::finish("POST /v1/credits/order", funnel.order),
        EndpointStats::finish("GET /v1/credits/order/:code", funnel.status_poll),
        EndpointStats::finish("POST reveal", funnel.reveal),
        EndpointStats::finish("POST ack", funnel.ack),
        EndpointStats::finish("GET /v1/models", t.models),
        EndpointStats::finish("GET /admin/api/credits/quota", t.plan),
        EndpointStats::finish("GET /admin/api/models/catalog", t.plans),
        EndpointStats::finish("GET /admin/api/usage/summary", t.summary),
        EndpointStats::finish("GET /admin/api/usage/by-model", t.by_model),
        EndpointStats::finish("GET /admin/api/usage/by-day", t.by_day),
        EndpointStats::finish("GET /admin/api/usage/quota", t.quota),
    ];
    let mut ttft: Vec<_> = stream_results.iter().filter_map(|s| s.ttft_us).collect();
    ttft.sort_unstable();
    let mut gaps: Vec<_> = stream_results.iter().map(|s| s.max_gap_us).collect();
    gaps.sort_unstable();
    let mut hbg = heartbeat.gaps_us.lock().unwrap().clone();
    hbg.sort_unstable();
    let stream_ok = stream_results
        .iter()
        .filter(|s| s.status == 200 && s.done)
        .count();
    let success = stream_ok == cfg.streams
        && complete == cfg.orders
        && delivered.len() == cfg.orders
        && endpoints.iter().all(|e| e.failed == 0)
        && audit.is_consistent()
        && audit.paid_orders == (cfg.orders + 16) as i64
        && delivery.1 == delivery.0
        && delivery.2 == 0
        && watcher_errors.is_empty();
    let report = json!({"success":success,"isolated":true,"counts":{"streams":cfg.streams,"orders":cfg.orders,"reads":cfg.reads,"purchased_existing_users":16,"seed_usage":cfg.seed_usage,"seed_catalog":cfg.seed_catalog,"keys":cfg.keys,"read_concurrency":cfg.read_concurrency,"workers":cfg.workers,"sqlite_synchronous":cfg.synchronous},"elapsed_seconds":elapsed.as_secs_f64(),"streams":{"ok":stream_ok,"failed":cfg.streams-stream_ok,"peak_open":peak.peak.load(Ordering::Relaxed),"ttft_p50_us":percentile(&ttft,0.5),"ttft_p99_us":percentile(&ttft,0.99),"max_gap_p99_us":percentile(&gaps,0.99),"max_gap_us":gaps.last(),"statuses":stream_results.iter().fold(std::collections::BTreeMap::<u16,usize>::new(),|mut a,s|{*a.entry(s.status).or_default()+=1;a})},"purchases":{"complete":complete,"unique_delivered_keys":delivered.len(),"watcher_credited_including_setup":credited,"watcher_errors":watcher_errors},"ledger":{"consistent":audit.is_consistent(),"paid_orders":audit.paid_orders,"topup_rows":audit.topup_rows,"delivery_rows":delivery.0,"acked":delivery.1,"ciphertexts_remaining":delivery.2},"heartbeat":{"stalls_over_55ms":hbg.len(),"p99_gap_us":percentile(&hbg,0.99),"max_gap_us":hbg.last()},"endpoints":endpoints});
    if profiling {
        std::fs::write(
            format!("{}.sql.json", cfg.json),
            serde_json::to_vec_pretty(&openproxy_db::sql_profile::stop())?,
        )?;
    }
    let text = serde_json::to_string_pretty(&report)?;
    if cfg.json == "-" {
        println!("{text}");
    } else {
        std::fs::write(&cfg.json, text)?;
        println!("report: {} success={success}", cfg.json);
    }
    anyhow::ensure!(success, "mixed load failed acceptance criteria (see JSON)");
    Ok(())
}
