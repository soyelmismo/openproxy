use super::*;

static NETFILTER_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[test]
fn script_scopes_notrack_to_loopback_and_given_ports() {
    let s = script("t", &[8788, 8787, 8787]);
    assert!(s.starts_with("add table inet t { flags owner; "));
    assert_eq!(s.lines().count(), 1, "install must be atomic");
    assert!(s.contains("type filter hook output priority raw; policy accept;"));
    assert!(s.contains("oifname \"lo\" tcp dport { 8787, 8788 } notrack"));
    assert!(s.contains("oifname \"lo\" tcp sport { 8787, 8788 } notrack"));
    assert_eq!(s.matches("notrack").count(), 2);
    assert!(!s.contains("iifname"));
    assert!(s.lines().all(|l| l.starts_with("add ")));
}

fn require_private_netns() {
    assert_ne!(
        std::fs::read_link("/proc/self/ns/net").unwrap(),
        std::fs::read_link("/proc/1/ns/net").unwrap(),
        "refusing to touch the host network namespace; use `unshare -n`"
    );
}

#[test]
fn table_name_is_unique_per_process() {
    assert_ne!(table_name(1), table_name(2));
    assert!(table_name(42).ends_with("_42"));
}

/// Touches netfilter, so it only runs inside a private network namespace:
/// `unshare -n cargo test -p openproxy-server --lib loopback_notrack -- --ignored`
#[tokio::test]
#[ignore = "needs CAP_NET_ADMIN inside `unshare -n`; never run on the host namespace"]
async fn table_lives_exactly_as_long_as_the_guard() {
    require_private_netns();
    let _lock = NETFILTER_TEST_LOCK.lock().await;
    let list = || async {
        let out = Command::new("nft")
            .args(["list", "tables"])
            .output()
            .await
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert!(
        !list().await.contains("openproxy_loopback_notrack"),
        "refusing to run outside an empty netns"
    );
    let guard = LoopbackNotrack::install(&[8787]).await.unwrap();
    let name = guard.table().to_owned();
    assert!(list().await.contains(&name));
    let output = Command::new("nft")
        .args(["-j", "list", "table", "inet", &name])
        .output()
        .await
        .unwrap();
    let mut json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(installed_exactly(&json, &name, &[8787]));
    json["nftables"].as_array_mut().unwrap().pop();
    assert!(
        !installed_exactly(&json, &name, &[8787]),
        "one direction is unsafe"
    );

    let other = Command::new("nft")
        .args(["delete", "table", "inet", &name])
        .output()
        .await
        .unwrap();
    assert!(
        !other.status.success(),
        "owner table must reject foreign edits"
    );

    guard.remove().await;
    assert!(!list().await.contains(&name));
}

#[tokio::test]
#[ignore = "needs CAP_NET_ADMIN inside `unshare -n`; never run on the host namespace"]
async fn dropping_the_guard_also_removes_the_table() {
    require_private_netns();
    let _lock = NETFILTER_TEST_LOCK.lock().await;
    let guard = LoopbackNotrack::install(&[8787]).await.unwrap();
    let name = guard.table().to_owned();
    drop(guard);
    for _ in 0..40 {
        let out = Command::new("nft")
            .args(["list", "tables"])
            .output()
            .await
            .unwrap();
        if !String::from_utf8_lossy(&out.stdout).contains(&name) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("table survived its guard");
}
