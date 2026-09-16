//! Image generation service: routing resolution, multi-target dispatch, and usage recording.

pub mod dispatch;
pub mod horde_poll;
pub mod multipart;
pub mod multipart_exec;
pub mod png_mask;

#[cfg(test)]
mod tests;

use std::time::Instant;

use openproxy_db::DbPool;
use openproxy_types::{EndpointKind, Result, ids::ApiKeyId};

use crate::routing::RoutingPlan;

pub use dispatch::{dispatch_image_request, execute_image_generation};
pub use multipart::{
    ImageMultipartKind, ImageServiceContext, MultipartFile, ParsedImageMultipartBody,
    dispatch_image_multipart_request, execute_image_edit, execute_image_variation,
};
pub use png_mask::extract_png_alpha_mask;

pub use crate::unary::{
    UnaryTarget as ImageTargets, UnaryTarget, UnaryUsageArgs, apply_adapter_headers,
    is_target_available, map_upstream_status_error, record_unary_usage, resolve_api_key,
    resolve_unary_targets,
};

pub type ImageUsageArgs<'a> = UnaryUsageArgs<'a>;

pub fn resolve_image_targets(
    db_pool: &DbPool,
    routing_plan: RoutingPlan,
    req_model: &str,
    api_key_id: Option<ApiKeyId>,
    started: Instant,
) -> Result<Vec<ImageTargets>> {
    let targets = resolve_unary_targets(
        db_pool,
        routing_plan,
        req_model,
        EndpointKind::Image,
        api_key_id,
        started,
    )?;
    Ok(consolidate_image_targets(targets))
}

fn merge_horde_target(existing: &mut ImageTargets, target: &ImageTargets) {
    let already_present = existing
        .upstream_model
        .split(',')
        .map(str::trim)
        .any(|m| m == target.upstream_model.as_str());

    if !already_present {
        if existing.upstream_model.is_empty() {
            existing.upstream_model = target.upstream_model.clone();
        } else {
            existing.upstream_model.push(',');
            existing.upstream_model.push_str(&target.upstream_model);
        }
    }
}

pub fn consolidate_image_targets(targets: Vec<ImageTargets>) -> Vec<ImageTargets> {
    let mut consolidated: Vec<ImageTargets> = Vec::new();
    for target in targets {
        let is_horde = target.provider.as_str() == "horde";
        let existing = if is_horde {
            consolidated
                .iter_mut()
                .find(|t| t.provider.as_str() == "horde" && t.account_id == target.account_id)
        } else {
            None
        };

        if let Some(existing) = existing {
            merge_horde_target(existing, &target);
        } else {
            consolidated.push(target);
        }
    }
    consolidated
}
