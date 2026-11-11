use crate::harness::TestHarness;
use axum::http::{StatusCode, header};
use openproxy_compression::{CompressionMode, apply_compression, would_compress};
use serde_json::{Value, json};

use super::msg;

#[test]
fn test_tier1_admin_usage_summary() {
    crate::run_with_big_stack(test_tier1_admin_usage_summary_body);
}

async fn test_tier1_admin_usage_summary_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/usage/summary").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        resp.get("unique_requests")
            .or_else(|| resp.get("total_rows"))
            .is_some()
    );
}

#[test]
fn test_tier1_admin_usage_by_model() {
    crate::run_with_big_stack(test_tier1_admin_usage_by_model_body);
}

async fn test_tier1_admin_usage_by_model_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/usage/by-model").await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.is_array());
}

#[test]
fn test_tier1_admin_usage_by_provider() {
    crate::run_with_big_stack(test_tier1_admin_usage_by_provider_body);
}

async fn test_tier1_admin_usage_by_provider_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/usage/by-provider").await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.is_array());
}

#[test]
fn test_tier1_admin_usage_recent_activity() {
    crate::run_with_big_stack(test_tier1_admin_usage_recent_activity_body);
}

async fn test_tier1_admin_usage_recent_activity_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/usage/recent").await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.is_array());
}

#[test]
fn test_tier1_admin_usage_latency() {
    crate::run_with_big_stack(test_tier1_admin_usage_latency_body);
}

async fn test_tier1_admin_usage_latency_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/usage/latency").await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.get("p50").is_some() || resp.is_object());
}

#[test]
fn test_tier1_admin_keys_list() {
    crate::run_with_big_stack(test_tier1_admin_keys_list_body);
}

async fn test_tier1_admin_keys_list_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/keys").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!resp.as_array().unwrap().is_empty());
}

#[test]
fn test_tier1_admin_keys_create() {
    crate::run_with_big_stack(test_tier1_admin_keys_create_body);
}

async fn test_tier1_admin_keys_create_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .admin_post(
            "/admin/api/keys",
            json!({"label": "tier1-new-key", "scopes": ["chat"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.get("plaintext").is_some());
    assert_eq!(resp["key"]["label"], "tier1-new-key");
}

#[test]
fn test_tier1_admin_keys_get_by_id() {
    crate::run_with_big_stack(test_tier1_admin_keys_get_by_id_body);
}

async fn test_tier1_admin_keys_get_by_id_body() {
    let harness = TestHarness::new().await;
    let id = harness.client_key_id.0;
    let (status, resp) = harness.admin_get(&format!("/admin/api/keys/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["id"], id);
}

#[test]
fn test_tier1_admin_keys_update_label() {
    crate::run_with_big_stack(test_tier1_admin_keys_update_label_body);
}

async fn test_tier1_admin_keys_update_label_body() {
    let harness = TestHarness::new().await;
    let id = harness.client_key_id.0;
    let (status, resp) = harness
        .admin_patch(
            &format!("/admin/api/keys/{id}"),
            json!({"label": "renamed-client-key"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["id"], id);
    let (_, key_info) = harness.admin_get(&format!("/admin/api/keys/{id}")).await;
    assert_eq!(key_info["label"], "renamed-client-key");
}

#[test]
fn test_tier1_admin_keys_revoke() {
    crate::run_with_big_stack(test_tier1_admin_keys_revoke_body);
}

async fn test_tier1_admin_keys_revoke_body() {
    let harness = TestHarness::new().await;
    let (_, new_key) = harness
        .admin_post(
            "/admin/api/keys",
            json!({"label": "revokable-key", "scopes": ["chat"]}),
        )
        .await;
    let id = new_key["key"]["id"].as_i64().unwrap();
    let (status, resp) = harness
        .admin_post(&format!("/admin/api/keys/{id}/revoke"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["revoked"], true);
}

#[test]
fn test_tier1_streaming_sse_frame_format() {
    crate::run_with_big_stack(test_tier1_streaming_sse_frame_format_body);
}

async fn test_tier1_streaming_sse_frame_format_body() {
    let harness = TestHarness::new().await;
    let payload = json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": "Stream frame test"}], "stream": true});
    let (status, _, body) = harness.client_chat_raw(payload).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("data: "));
}

#[test]
fn test_tier1_streaming_token_chunks() {
    crate::run_with_big_stack(test_tier1_streaming_token_chunks_body);
}

async fn test_tier1_streaming_token_chunks_body() {
    let harness = TestHarness::new().await;
    let payload = json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": "Tokens stream test"}], "stream": true});
    let (status, _, body) = harness.client_chat_raw(payload).await;
    assert_eq!(status, StatusCode::OK);
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("Hello") && body_str.contains("from mock!"));
}

#[test]
fn test_tier1_streaming_done_terminator() {
    crate::run_with_big_stack(test_tier1_streaming_done_terminator_body);
}

async fn test_tier1_streaming_done_terminator_body() {
    let harness = TestHarness::new().await;
    let payload = json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": "Done terminator test"}], "stream": true});
    let (status, _, body) = harness.client_chat_raw(payload).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("data: [DONE]"));
}

#[test]
fn test_tier1_streaming_content_type() {
    crate::run_with_big_stack(test_tier1_streaming_content_type_body);
}

async fn test_tier1_streaming_content_type_body() {
    let harness = TestHarness::new().await;
    let payload = json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": "Content type test"}], "stream": true});
    let (status, headers, _) = harness.client_chat_raw(payload).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("text/event-stream")
    );
}

#[test]
fn test_tier1_streaming_multi_chunk_assembly() {
    crate::run_with_big_stack(test_tier1_streaming_multi_chunk_assembly_body);
}

async fn test_tier1_streaming_multi_chunk_assembly_body() {
    let harness = TestHarness::new().await;
    let payload = json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": "Multi chunk assemble"}], "stream": true});
    let (status, _, body) = harness.client_chat_raw(payload).await;
    assert_eq!(status, StatusCode::OK);
    let mut assembled = String::new();
    for line in String::from_utf8_lossy(&body).lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if data.trim() == "[DONE]" {
                break;
            }
            if let Ok(chunk) = serde_json::from_str::<Value>(data)
                && let Some(content) = chunk["choices"][0]["delta"]["content"].as_str()
            {
                assembled.push_str(content);
            }
        }
    }
    assert_eq!(assembled, "Hello from mock!");
}

#[test]
fn test_tier1_compression_lite_repeated_content() {
    let text = "You are an AI assistant.\n\n\n\n\n\nAlways provide helpful answers.";
    let mut msgs = vec![
        msg("system", text),
        msg("system", text),
        msg(
            "user",
            "Please help me with this problem.\n\n\n\n\nThank you!",
        ),
    ];
    let stats = apply_compression(&mut msgs, CompressionMode::Lite);
    assert!(!stats.techniques.is_empty() || stats.compressed_chars < stats.original_chars);
}

#[test]
fn test_tier1_compression_rtk_git_status() {
    let git_status = "On branch main\nYour branch is up to date with 'origin/main'.\n\nChanges not staged for commit:\n  (use \"git add <file>...\" to update what will be committed)\n  (use \"git restore <file>...\" to discard changes in working directory)\n\tmodified:   src/lib.rs\n\tmodified:   Cargo.toml\n\nno changes added to commit (use \"git add\" to commit)\n".repeat(5);
    let mut msgs = vec![msg("user", &git_status)];
    assert!(would_compress(&msgs, CompressionMode::Rtk));
    let stats = apply_compression(&mut msgs, CompressionMode::Rtk);
    assert!(!stats.techniques.is_empty() || stats.compressed_chars < stats.original_chars);
}

#[test]
fn test_tier1_compression_rtk_diff_output() {
    let diff_text = "diff --git a/src/main.rs b/src/main.rs\nindex 83a0022..b320d41 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10,6 +10,8 @@ fn main() {\n-    println!(\"old text\");\n+    println!(\"new text\");\n".repeat(10);
    let mut msgs = vec![msg("user", &diff_text)];
    assert!(would_compress(&msgs, CompressionMode::Rtk));
    let stats = apply_compression(&mut msgs, CompressionMode::Rtk);
    assert!(!stats.techniques.is_empty() || stats.compressed_chars < stats.original_chars);
}

#[test]
fn test_tier1_compression_litertk_combined() {
    let mixed_payload = format!(
        "{}\n{}",
        "repeated whitespace     and spaces     \n".repeat(20),
        "diff --git a/test b/test\n--- a/test\n+++ b/test\n@@ -1 +1 @@\n-old\n+new\n".repeat(10)
    );
    let mut msgs = vec![msg("user", &mixed_payload)];
    assert!(would_compress(&msgs, CompressionMode::LiteRtk));
    let stats = apply_compression(&mut msgs, CompressionMode::LiteRtk);
    assert!(!stats.techniques.is_empty() || stats.compressed_chars < stats.original_chars);
}

#[test]
fn test_tier1_compression_threshold_skip() {
    let msgs = vec![msg("user", "Hello short text")];
    assert!(
        !would_compress(&msgs, CompressionMode::Lite)
            && !would_compress(&msgs, CompressionMode::Rtk)
    );
}
