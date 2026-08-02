sed -i 's/\.map_err(|e| ApiError(CoreError::Internal(format!("spawn_blocking failed: {}", e))))??;/.unwrap_or_else(|e| Err(CoreError::Internal(format!("spawn_blocking failed: {}", e))))?;/g' crates/openproxy-server/src/handlers/admin/oauth.rs
sed -i 's/\.map_err(|e| {/.unwrap_or_else(|e| {/g' crates/openproxy-server/src/handlers/admin/oauth.rs
sed -i 's/    ApiError(CoreError::Internal(format!("spawn_blocking failed: {}", e)))/    Err(CoreError::Internal(format!("spawn_blocking failed: {}", e)))/g' crates/openproxy-server/src/handlers/admin/oauth.rs
sed -i 's/})??;/})?;/g' crates/openproxy-server/src/handlers/admin/oauth.rs
sed -i 's/})??/})?/g' crates/openproxy-server/src/handlers/admin/oauth.rs
