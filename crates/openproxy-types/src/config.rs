use serde::{Deserialize, Serialize};

impl_string_enum! {
    #[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
    #[serde(rename_all = "snake_case")]
    pub enum CompressionMode {
        #[default]
        Off => "off",
        Lite => "lite",
        Rtk => "rtk",
        LiteRtk => "lite_rtk",
    }
    error: "compression_mode"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    /// Separate administration listener; absent preserves the single-port layout.
    #[serde(default)]
    pub admin_bind: Option<String>,
    pub request_max_body_bytes: usize,
    #[serde(default = "default_rate_limit_requests")]
    pub rate_limit_requests_per_minute: u32,
    /// DEPRECATED and ignored since 2026-11-11: the anonymous first-boot
    /// window was the only path that could produce unattributed (keyless)
    /// pipeline runs. Kept for config compatibility; every /v1 request now
    /// requires an api key (bootstrap key covers first-boot operator access).
    #[serde(default = "default_allow_anonymous")]
    pub allow_anonymous: bool,
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// Prepaid credits (top-up with on-chain USDC, expiring balance).
    /// Absent or `watch: false` disables the on-chain watcher; the REST
    /// surface reports `disabled`.
    #[serde(default)]
    pub credits: Option<CreditsConfig>,
}

/// Single-address USDC top-up funnel (Base L2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditsConfig {
    /// JSON-RPC endpoint of a Base node used to watch incoming transfers.
    pub rpc_url: String,
    /// USDC token contract on Base (`0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913`).
    pub usdc_address: String,
    /// The single collector address every top-up is sent to (the funnel).
    pub collector_address: String,
    /// Payment window of an order, in minutes (TipRouter-style: 15).
    #[serde(default = "default_payment_window_minutes")]
    pub payment_window_minutes: u32,
    /// Watcher poll interval in seconds.
    #[serde(default = "default_scan_interval_secs")]
    pub scan_interval_secs: u64,
    /// Allowed balance lifetimes for a top-up, in days.
    #[serde(default = "default_ttl_days")]
    pub ttl_days: Vec<u32>,
    /// EVM chain id of the network the collector lives on (Base = 8453).
    #[serde(default = "default_chain_id")]
    pub chain_id: u64,
    /// Largest block span a single `eth_getLogs` call may ask for. Public
    /// providers cap this (Base's blastapi endpoint rejects anything above 10
    /// blocks), and a range the node refuses means the watcher never advances
    /// its cursor. Lower it to the node's limit, never above it.
    #[serde(default = "default_max_block_chunk")]
    pub max_block_chunk: u32,
    /// How far a received transfer may sit from an order's quoted amount and
    /// still pay it, in micro-USDC. Custodial exchanges (Binance) deduct their
    /// withdrawal fee from the sent amount, so a transfer arrives short — an
    /// exact-only matcher strands that money in the collector. The default
    /// (250_000 = 0.25 USDC) covers the ~0.2 USDC Base withdrawal fee with
    /// headroom; a transfer short by more falls back to `unmatched`.
    #[serde(default = "default_payment_tolerance_units")]
    pub payment_tolerance_units: i64,
    /// Checkout admission control (root fix for the jitter-space exhaustion
    /// DoS): maximum order creations (`POST /v1/credits/order` and
    /// `POST /v1/credits/me/plan`) per client IP inside
    /// `checkout_order_window_secs`. Each order reserves a unique live
    /// amount for its whole window + confirmation grace, and there are only
    /// 10 000 jitter slots per base price, so unbounded free creation lets
    /// one origin close the sales funnel of a price (~10k orders, reproduced)
    /// and squat the amount space. The default caps one IP at 30 orders per
    /// 10 minutes (≈ 75 live orders in steady state — 0.75% of a price's
    /// slot space).
    #[serde(default = "default_checkout_order_creations_per_ip")]
    pub checkout_order_creations_per_ip: u32,
    /// Window for [`CreditsConfig::checkout_order_creations_per_ip`], in
    /// seconds.
    #[serde(default = "default_checkout_order_window_secs")]
    pub checkout_order_window_secs: u64,
    /// Checkout admission control for the read/reveal surface
    /// (`GET /v1/credits/order/{code}`, `reveal`, `ack`, `me/plan` GET,
    /// `me/rotate`): maximum requests per client IP per minute. The default
    /// (240) leaves the polling UI (one poll per 5 s = 12/min) two orders of
    /// magnitude of headroom while bounding receipt brute-force attempts.
    #[serde(default = "default_checkout_poll_per_ip_per_min")]
    pub checkout_poll_per_ip_per_min: u32,
}

/// Upper bound for [`CreditsConfig::payment_tolerance_units`] (µUSDC). The
/// tolerance is an exchange-fee allowance, not a discount policy: a value in
/// the millions would let one near-worthless transfer pay any live order, so
/// a typo this large must fail the load instead of silently repricing every
/// plan at "whatever arrived".
pub const MAX_PAYMENT_TOLERANCE_UNITS: i64 = 10_000_000;

/// Upper bound for [`CreditsConfig::max_block_chunk`]. Public providers
/// reject ranges above a few thousand blocks (some cap at 10), so a runaway
/// value only ever produces `eth_getLogs` errors; it is a misconfiguration,
/// not a throughput knob.
pub const MAX_BLOCK_CHUNK: u32 = 10_000;

/// `true` for a canonical 20-byte EVM address (`0x` + 40 hex characters).
fn is_evm_address(s: &str) -> bool {
    let body = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    body.len() == 40 && body.bytes().all(|b| b.is_ascii_hexdigit())
}

impl CreditsConfig {
    /// Money-path sanity (audit round 2, V-02/A4-07): these numbers feed the
    /// payment matcher and the chain scanner directly. An out-of-range value
    /// must fail the configuration load — the old code accepted anything and
    /// the failure surfaced later as a silently repriced plan (a giant
    /// tolerance matches any short payment to any live order) or a watcher
    /// that never advances its cursor (a chunk the node always rejects).
    ///
    /// The RPC endpoint is only checked for presence here: loopback HTTP is
    /// legitimate in tests and local deployments, so transport security stays
    /// an operational requirement rather than a load-time hard failure.
    ///
    /// # Errors
    /// Returns a human-readable message naming the offending field.
    pub fn validate(&self) -> Result<(), String> {
        if self.rpc_url.trim().is_empty() {
            return Err("credits.rpc_url must not be empty".into());
        }
        if !is_evm_address(&self.usdc_address) {
            return Err(format!(
                "credits.usdc_address must be a 20-byte hex address, got {:?}",
                self.usdc_address
            ));
        }
        if !is_evm_address(&self.collector_address) {
            return Err(format!(
                "credits.collector_address must be a 20-byte hex address, got {:?}",
                self.collector_address
            ));
        }
        if self.chain_id == 0 {
            return Err("credits.chain_id must be a real EVM chain id (got 0)".into());
        }
        if self.payment_window_minutes == 0 || self.payment_window_minutes > 1_440 {
            return Err(format!(
                "credits.payment_window_minutes must be 1..=1440, got {}",
                self.payment_window_minutes
            ));
        }
        if self.scan_interval_secs == 0 {
            return Err("credits.scan_interval_secs must be at least 1".into());
        }
        if self.max_block_chunk == 0 || self.max_block_chunk > MAX_BLOCK_CHUNK {
            return Err(format!(
                "credits.max_block_chunk must be 1..={MAX_BLOCK_CHUNK} blocks, got {}",
                self.max_block_chunk
            ));
        }
        if !(0..=MAX_PAYMENT_TOLERANCE_UNITS).contains(&self.payment_tolerance_units) {
            return Err(format!(
                "credits.payment_tolerance_units must be 0..={MAX_PAYMENT_TOLERANCE_UNITS} \
                 (µUSDC), got {}",
                self.payment_tolerance_units
            ));
        }
        if self.checkout_order_creations_per_ip == 0 {
            return Err("credits.checkout_order_creations_per_ip must be at least 1".into());
        }
        if self.checkout_order_window_secs == 0 {
            return Err("credits.checkout_order_window_secs must be at least 1".into());
        }
        if self.checkout_poll_per_ip_per_min == 0 {
            return Err("credits.checkout_poll_per_ip_per_min must be at least 1".into());
        }
        Ok(())
    }
}

fn default_max_block_chunk() -> u32 {
    500
}

fn default_checkout_order_creations_per_ip() -> u32 {
    30
}

fn default_checkout_order_window_secs() -> u64 {
    600
}

fn default_checkout_poll_per_ip_per_min() -> u32 {
    240
}

fn default_payment_tolerance_units() -> i64 {
    250_000
}

fn default_chain_id() -> u64 {
    8453
}

fn default_payment_window_minutes() -> u32 {
    15
}
fn default_scan_interval_secs() -> u64 {
    5
}
fn default_ttl_days() -> Vec<u32> {
    vec![7, 15, 30]
}

fn default_rate_limit_requests() -> u32 {
    1000
}

fn default_allow_anonymous() -> bool {
    false
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            // Security (OP-04): loopback by default. The binary speaks plain
            // HTTP only (no TLS support), so binding a public interface out of
            // the box would expose every credential — including admin Bearer
            // tokens — in cleartext. Deployments that need to expose the port
            // must set an explicit `server.bind` and put a TLS-terminating
            // reverse proxy in front (see the startup warning in main.rs).
            bind: "127.0.0.1:8787".into(),
            admin_bind: None,
            // Default preserved from the previously enforced hardcode (the
            // setting used to be dead); operators can lower it per deployment
            // now that it is actually wired into the enforced limits (OP-14).
            request_max_body_bytes: 32 * 1024 * 1024,
            rate_limit_requests_per_minute: 1000,
            allow_anonymous: false,
            trusted_proxies: Vec::new(),
            credits: None,
        }
    }
}

impl_string_enum! {
    /// SQLite synchronous pragma mode.
    ///
    /// Controls the aggressiveness of SQLite syncing transactions to disk via fsync.
    /// Default is `Full` for crash safety and journal durability.
    ///
    /// Durability note: `Full` depends on the underlying filesystem and hardware honoring
    /// flush/fsync requests; it does not grant absolute immunity to power loss.
    #[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
    #[serde(rename_all = "snake_case")]
    pub enum SqliteSynchronous {
        #[default]
        #[serde(alias = "FULL", alias = "Full")]
        Full => "full" | "FULL" | "Full",
        #[serde(alias = "NORMAL", alias = "Normal")]
        Normal => "normal" | "NORMAL" | "Normal",
    }
    error: "sqlite_synchronous"
}

impl SqliteSynchronous {
    /// PRAGMA synchronous value expected by SQLite SQL statements.
    #[must_use]
    pub const fn as_pragma_str(&self) -> &'static str {
        match self {
            Self::Full => "FULL",
            Self::Normal => "NORMAL",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub database_path: String,
    /// Independent SQLite readers. Zero selects an automatic bounded count.
    #[serde(default)]
    pub reader_count: usize,
    /// SQLite synchronous pragma mode (FULL by default for safety, or NORMAL for throughput).
    /// Note: FULL depends on underlying storage honoring fsync/flush; it does not grant
    /// absolute immunity to power loss.
    #[serde(default, alias = "sqlite_synchronous")]
    pub synchronous: SqliteSynchronous,
    pub encryption_key_source: EncryptionKeySource,
    /// Path to the base64-encoded 32-byte master key file, used when
    /// `encryption_key_source = "file"` (OP-29). Required in that mode;
    /// the file must have mode 0600.
    #[serde(default)]
    pub encryption_key_file: Option<String>,
    #[serde(default)]
    pub maintenance: MaintenanceConfig,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            database_path: "~/.openproxy/data.db".into(),
            reader_count: 0,
            synchronous: SqliteSynchronous::Full,
            encryption_key_source: EncryptionKeySource::Env,
            encryption_key_file: None,
            maintenance: MaintenanceConfig::default(),
        }
    }
}

impl_string_enum! {
    #[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "lowercase")]
    pub enum EncryptionKeySource {
        #[default]
        Env => "env",
        File => "file",
    }
    error: "encryption_key_source"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceConfig {
    #[serde(default = "default_auto_vacuum")]
    pub auto_vacuum: bool,
    #[serde(default = "default_maintenance_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_usage_retention_days")]
    pub usage_retention_days: u32,
    #[serde(default = "default_recording_ttl_secs")]
    pub recording_ttl_secs: i64,
}

fn default_auto_vacuum() -> bool {
    true
}
fn default_maintenance_interval_secs() -> u64 {
    6 * 3600
}
fn default_usage_retention_days() -> u32 {
    7
}
fn default_recording_ttl_secs() -> i64 {
    300
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            auto_vacuum: default_auto_vacuum(),
            interval_secs: default_maintenance_interval_secs(),
            usage_retention_days: default_usage_retention_days(),
            recording_ttl_secs: default_recording_ttl_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RacingConfig {
    pub default_race_size: u8,
    pub max_race_size: u8,
    pub abort_grace_ms: u64,
}

impl Default for RacingConfig {
    fn default() -> Self {
        Self {
            default_race_size: 1,
            max_race_size: 8,
            abort_grace_ms: 500,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimeoutsConfig {
    pub connect_ms: u64,
    pub request_send_ms: u64,
    pub ttft_ms: u64,
    pub idle_chunk_ms: u64,
    pub total_ms: u64,
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            connect_ms: 5_000,
            request_send_ms: 10_000,
            ttft_ms: 6_000,
            idle_chunk_ms: 120_000,
            total_ms: 300_000,
        }
    }
}

pub const IDLE_CHUNK_RETRYABLE_DEFAULT: bool = false;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RetriesConfig {
    pub max_attempts: u8,
    pub backoff_base_ms: u64,
    pub backoff_factor: u8,
    pub backoff_jitter_pct: u8,
    #[serde(default = "default_idle_chunk_retryable")]
    pub idle_chunk_retryable: bool,
    #[serde(default = "default_combo_max_attempts")]
    pub combo_max_attempts: u8,
}

fn default_idle_chunk_retryable() -> bool {
    IDLE_CHUNK_RETRYABLE_DEFAULT
}

fn default_combo_max_attempts() -> u8 {
    1
}

impl Default for RetriesConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff_base_ms: 200,
            backoff_factor: 2,
            backoff_jitter_pct: 50,
            idle_chunk_retryable: IDLE_CHUNK_RETRYABLE_DEFAULT,
            combo_max_attempts: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    pub failure_threshold: u8,
    pub unhealthy_duration_ms: u64,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            unhealthy_duration_ms: 60_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CooldownConfig {
    pub cooldown_secs: u64,
    #[serde(default = "default_cooldown_max_secs")]
    pub max_secs: u64,
    #[serde(default = "default_cooldown_factor")]
    pub factor: u32,
}

fn default_cooldown_max_secs() -> u64 {
    3600
}
fn default_cooldown_factor() -> u32 {
    2
}

impl Default for CooldownConfig {
    fn default() -> Self {
        Self {
            cooldown_secs: 60,
            max_secs: default_cooldown_max_secs(),
            factor: default_cooldown_factor(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaProtectionConfig {
    #[serde(default = "default_quota_protection_enabled")]
    pub enabled: bool,
    #[serde(default = "default_quota_protection_threshold")]
    pub threshold_percentage: u32,
}

fn default_quota_protection_enabled() -> bool {
    true
}
fn default_quota_protection_threshold() -> u32 {
    10
}

impl Default for QuotaProtectionConfig {
    fn default() -> Self {
        Self {
            enabled: default_quota_protection_enabled(),
            threshold_percentage: default_quota_protection_threshold(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModalityRoutingConfig {
    /// Drop combo targets that cannot accept the request's input modalities.
    ///
    /// Today only images are gated: when the payload carries `image_url` /
    /// `input_image` parts, targets whose model does not advertise `image`
    /// in `input_modalities` are removed before routing. Combos are
    /// advertised with the union of their leaves' capabilities, so without
    /// this gate a text-only first hop happily accepts a vision request and
    /// fails upstream with a non-retryable 400.
    #[serde(default = "default_modality_routing_enabled")]
    pub enabled: bool,
    /// Keep the unfiltered target set when no target accepts the request.
    ///
    /// A combo with no vision-capable leaf would otherwise return
    /// `no_healthy_targets`. With this on (the default) the gate logs the
    /// mismatch and falls back to the previous behaviour, letting the
    /// upstream reject the request instead of the proxy.
    #[serde(default = "default_modality_routing_fallback")]
    pub fallback_to_text_only: bool,
}

fn default_modality_routing_enabled() -> bool {
    true
}

fn default_modality_routing_fallback() -> bool {
    true
}

impl Default for ModalityRoutingConfig {
    fn default() -> Self {
        Self {
            enabled: default_modality_routing_enabled(),
            fallback_to_text_only: default_modality_routing_fallback(),
        }
    }
}

/// Masking of target failures before they leave the proxy.
///
/// A request routed through a combo must not learn which provider or model
/// served it, which account ran out of quota, or what the upstream body said:
/// that is operator-side information and, on a sold combo, the product itself.
/// With this on, any `4xx`/`5xx` a combo would answer leaves as a generic class
/// keyed on the status ([`crate::CoreError::masked_upstream`]): the caller's own
/// payload was refused (400/413/422), nothing matched (404), the upstream is
/// under pressure (429 with `Retry-After`), the model did not answer (504) or it
/// is unavailable (502).
///
/// The one family never masked is the caller's own quota/plan rejection
/// (`QuotaExceeded`, `InsufficientCredits`): this proxy produces it, never a
/// target, and it is the only actionable information a paying customer has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetErrorMaskingConfig {
    /// Replace target failures with a generic message when the request was
    /// routed through a real combo.
    #[serde(default = "default_target_error_masking_enabled")]
    pub enabled: bool,
}

fn default_target_error_masking_enabled() -> bool {
    true
}

impl Default for TargetErrorMaskingConfig {
    fn default() -> Self {
        Self {
            enabled: default_target_error_masking_enabled(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartWarmupConfig {
    #[serde(default = "default_smart_warmup_enabled")]
    pub enabled: bool,
    #[serde(default = "default_smart_warmup_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_smart_warmup_models")]
    pub models: Box<[String]>,
}

fn default_smart_warmup_enabled() -> bool {
    true
}
fn default_smart_warmup_interval() -> u64 {
    3600
}

fn default_smart_warmup_models() -> Box<[String]> {
    vec!["gemini-pro".to_string(), "claude".to_string()].into_boxed_slice()
}

impl Default for SmartWarmupConfig {
    fn default() -> Self {
        Self {
            enabled: default_smart_warmup_enabled(),
            interval_secs: default_smart_warmup_interval(),
            models: default_smart_warmup_models(),
        }
    }
}

impl_string_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Hash)]
    #[serde(rename_all = "snake_case")]
    pub enum CooldownMode {
        #[default]
        Flat => "flat",
        Exponential => "exponential",
        None => "none" | "disabled" | "off",
    }
    error: "cooldown_mode"
}

impl_string_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
    #[serde(rename_all = "snake_case")]
    pub enum PiiEntity {
        Email => "email",
        Phone => "phone",
        #[serde(alias = "ipv4", alias = "ipv6")]
        Ip => "ip" | "ipv4" | "ipv6",
        #[serde(alias = "card")]
        CreditCard => "credit_card" | "card",
        #[serde(alias = "key", alias = "api_key")]
        Secret => "secret" | "key" | "api_key",
        #[serde(alias = "name")]
        Person => "person" | "name",
    }
    core_error: "pii_entity"
}

impl PiiEntity {
    pub const ALL: [Self; 6] = [
        Self::Email,
        Self::Phone,
        Self::Ip,
        Self::CreditCard,
        Self::Secret,
        Self::Person,
    ];

    #[must_use]
    pub const fn placeholder_prefix(self) -> &'static str {
        match self {
            Self::Email => "EMAIL",
            Self::Phone => "PHONE",
            Self::Ip => "IP",
            Self::CreditCard => "CARD",
            Self::Secret => "KEY",
            Self::Person => "PERSON",
        }
    }
}

fn deserialize_dedup_entities<'de, D>(deserializer: D) -> Result<Vec<PiiEntity>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let list = Vec::<PiiEntity>::deserialize(deserializer)?;
    let mut seen = std::collections::HashSet::with_capacity(list.len());
    let mut out = Vec::with_capacity(list.len());
    for item in list {
        if seen.insert(item) {
            out.push(item);
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiiConfig {
    #[serde(default = "default_pii_enabled", alias = "enabled")]
    pub pii_enabled: bool,
    #[serde(default = "default_pii_reversible", alias = "reversible")]
    pub pii_reversible: bool,
    #[serde(default = "default_pii_redact_logs", alias = "redact_logs")]
    pub pii_redact_logs: bool,
    #[serde(
        default = "default_pii_entities",
        alias = "entities",
        deserialize_with = "deserialize_dedup_entities"
    )]
    pub pii_entities: Vec<PiiEntity>,
}

fn default_pii_enabled() -> bool {
    false
}

fn default_pii_reversible() -> bool {
    true
}

fn default_pii_redact_logs() -> bool {
    true
}

fn default_pii_entities() -> Vec<PiiEntity> {
    PiiEntity::ALL.to_vec()
}

impl Default for PiiConfig {
    fn default() -> Self {
        Self {
            pii_enabled: default_pii_enabled(),
            pii_reversible: default_pii_reversible(),
            pii_redact_logs: default_pii_redact_logs(),
            pii_entities: default_pii_entities(),
        }
    }
}

#[cfg(test)]
mod tests;
