use crate::adapters::{Arc, CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest};

pub const ZAI_BIZ_CUSTOMER_INFO_URL: &str = "https://api.z.ai/api/biz/customer/getCustomerInfo";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZaiOrgProject {
    pub organization_id: String,
    pub project_id: String,
}

/// Extracts the canonical default organization and project from `getCustomerInfo` JSON payload.
pub fn pick_org_and_project(data: &serde_json::Value) -> Option<ZaiOrgProject> {
    let orgs = data.get("organizations")?.as_array()?;

    let mut candidate_orgs = Vec::new();
    for org in orgs {
        let org_id = org
            .get("organizationId")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)?;
        if org_id.is_empty() {
            continue;
        }
        let org_name = org
            .get("organizationName")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let is_default = org
            .get("isDefault")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let Some(projects) = org.get("projects").and_then(serde_json::Value::as_array) else {
            continue;
        };

        let mut valid_projects = Vec::new();
        for proj in projects {
            let p_id = match proj
                .get("projectId")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
            {
                Some(id) if !id.is_empty() => id,
                _ => continue,
            };
            let p_type = proj
                .get("projectType")
                .map(|t| t.to_string())
                .unwrap_or_default();
            if p_type == "2" || p_type == "\"2\"" {
                continue;
            }
            let p_name = proj
                .get("projectName")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let p_def = proj
                .get("isDefault")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);

            valid_projects.push((p_id.to_string(), p_name.to_string(), p_def));
        }

        if !valid_projects.is_empty() {
            candidate_orgs.push((
                org_id.to_string(),
                org_name.to_string(),
                is_default,
                valid_projects,
            ));
        }
    }

    if candidate_orgs.is_empty() {
        return None;
    }

    let selected_org = candidate_orgs
        .iter()
        .find(|(_, name, def, _)| *def || name.contains("默认机构"))
        .unwrap_or(&candidate_orgs[0]);

    let selected_proj = selected_org
        .3
        .iter()
        .find(|(_, name, def)| *def || name.contains("默认项目"))
        .unwrap_or(&selected_org.3[0]);

    Some(ZaiOrgProject {
        organization_id: selected_org.0.clone(),
        project_id: selected_proj.0.clone(),
    })
}

/// Resolves the canonical Anthropic-compatible API key (`<apiKey>.<secretKey>`) for Z.ai (ZCode).
pub async fn resolve_zai_api_key(
    upstream: &Arc<UpstreamClient>,
    business_token: &str,
) -> Option<String> {
    let token = business_token.trim();
    if token.is_empty() {
        return None;
    }

    // 1. Fetch customer info to identify default organization and project
    let mut info_req = UpstreamRequest::get(ZAI_BIZ_CUSTOMER_INFO_URL);
    if let Ok(val) = http::HeaderValue::from_str(&format!("Bearer {token}")) {
        info_req.headers.insert(http::header::AUTHORIZATION, val);
    }
    info_req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    info_req.headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("ZCode/3.14.0"),
    );

    let cancel = CancellationToken::new();
    let info_resp = upstream
        .call(info_req, TimeoutProfile::OAuth, cancel)
        .await
        .ok()?;
    if !info_resp.status.is_success() {
        return None;
    }

    let info_bytes = info_resp.collect().await.ok()?;
    let info_val: serde_json::Value = serde_json::from_slice(&info_bytes).ok()?;
    let data_val = info_val.get("data")?;
    let org_proj = pick_org_and_project(data_val)?;

    let base_api_keys_url = format!(
        "https://api.z.ai/api/biz/v1/organization/{}/projects/{}/api_keys",
        org_proj.organization_id, org_proj.project_id
    );

    // 2. Query existing API keys
    let mut list_req = UpstreamRequest::get(&base_api_keys_url);
    if let Ok(val) = http::HeaderValue::from_str(&format!("Bearer {token}")) {
        list_req.headers.insert(http::header::AUTHORIZATION, val);
    }
    list_req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    list_req.headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("ZCode/3.14.0"),
    );

    let cancel_list = CancellationToken::new();
    let list_resp = upstream
        .call(list_req, TimeoutProfile::OAuth, cancel_list)
        .await
        .ok()?;

    let mut api_key_opt: Option<String> = None;

    if list_resp.status.is_success()
        && let Ok(list_bytes) = list_resp.collect().await
        && let Ok(list_json) = serde_json::from_slice::<serde_json::Value>(&list_bytes)
    {
        let items: Option<&Vec<serde_json::Value>> = list_json
            .get("data")
            .and_then(serde_json::Value::as_array)
            .or_else(|| {
                list_json
                    .get("data")
                    .and_then(|d| d.get("list"))
                    .and_then(serde_json::Value::as_array)
            });

        if let Some(arr) = items {
            // Prefer key explicitly named "zcode-api-key"
            let found = arr
                .iter()
                .find(|item| {
                    item.get("name").and_then(serde_json::Value::as_str) == Some("zcode-api-key")
                })
                .or_else(|| {
                    // Fallback to first item with valid non-empty apiKey
                    arr.iter().find(|item| {
                        item.get("apiKey")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|k| !k.trim().is_empty())
                    })
                });

            if let Some(entry) = found {
                api_key_opt = entry
                    .get("apiKey")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|k| !k.is_empty())
                    .map(ToString::to_string);
            }
        }
    }

    // 3. If no key found, create one via POST
    if api_key_opt.is_none() {
        let body = serde_json::json!({ "name": "zcode-api-key" });
        if let Ok(body_bytes) = serde_json::to_vec(&body) {
            let mut create_req =
                UpstreamRequest::post_json(&base_api_keys_url, bytes::Bytes::from(body_bytes));
            if let Ok(val) = http::HeaderValue::from_str(&format!("Bearer {token}")) {
                create_req.headers.insert(http::header::AUTHORIZATION, val);
            }
            create_req.headers.insert(
                http::header::ACCEPT,
                http::HeaderValue::from_static("application/json"),
            );
            create_req.headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            create_req.headers.insert(
                http::header::USER_AGENT,
                http::HeaderValue::from_static("ZCode/3.14.0"),
            );

            let cancel_create = CancellationToken::new();
            if let Ok(create_resp) = upstream
                .call(create_req, TimeoutProfile::OAuth, cancel_create)
                .await
                && create_resp.status.is_success()
                && let Ok(create_bytes) = create_resp.collect().await
                && let Ok(create_json) = serde_json::from_slice::<serde_json::Value>(&create_bytes)
            {
                api_key_opt = create_json
                    .get("data")
                    .and_then(|d| d.get("apiKey"))
                    .or_else(|| create_json.get("apiKey"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|k| !k.is_empty())
                    .map(ToString::to_string);
            }
        }
    }

    let raw_api_key = api_key_opt?;

    // 4. Retrieve secret key via copy endpoint
    let copy_url = format!("{base_api_keys_url}/copy/{raw_api_key}");
    let mut copy_req = UpstreamRequest::get(&copy_url);
    if let Ok(val) = http::HeaderValue::from_str(&format!("Bearer {token}")) {
        copy_req.headers.insert(http::header::AUTHORIZATION, val);
    }
    copy_req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    copy_req.headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("ZCode/3.14.0"),
    );

    let cancel_copy = CancellationToken::new();
    if let Ok(copy_resp) = upstream
        .call(copy_req, TimeoutProfile::OAuth, cancel_copy)
        .await
        && copy_resp.status.is_success()
        && let Ok(copy_bytes) = copy_resp.collect().await
        && let Ok(copy_json) = serde_json::from_slice::<serde_json::Value>(&copy_bytes)
    {
        let secret_key = copy_json
            .get("data")
            .and_then(|d| d.get("secretKey"))
            .or_else(|| copy_json.get("secretKey"))
            .and_then(serde_json::Value::as_str)
            .map_or("", str::trim);

        if !secret_key.is_empty() {
            return Some(format!("{raw_api_key}.{secret_key}"));
        }
    }

    Some(raw_api_key)
}
