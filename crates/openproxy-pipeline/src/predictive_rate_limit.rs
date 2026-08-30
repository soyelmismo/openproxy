use openproxy_types::combos::ComboTarget;
use openproxy_types::ids::{AccountId, ComboId, ComboTargetId, ModelRowId, ProviderId};
use openproxy_types::providers::RateLimitScope;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

const SHARD_COUNT: usize = 256;
const DEFAULT_BURST_CAPACITY: u16 = 1000;
const DEFAULT_WINDOW_DURATION_MS: u32 = 60_000;
const PROBE_SUCCESS_THRESHOLD: u16 = 2;
const MIN_RECOVERED_BURST: u16 = 2;
const MEMORY_DECAY_IDLE_MS: u64 = 15 * 60 * 1000; // 15 min sin 429s -> decay

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetRateState {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug, Clone)]
pub struct TargetPredictiveState {
    pub window_start_ms: u64,
    pub reset_at_ms: u64,
    pub last_429_at_ms: u64,
    pub last_success_at_ms: u64,
    pub last_error_fingerprint: u64,
    pub window_duration_ms: u32,
    pub learned_burst: u16,
    pub window_count: u16,
    pub in_flight: u16,
    pub success_streak: u16,
    pub consecutive_failures: u8,
    pub consecutive_same_fingerprint: u8,
    pub state: TargetRateState,
}

impl Default for TargetPredictiveState {
    fn default() -> Self {
        Self {
            window_start_ms: 0,
            reset_at_ms: 0,
            last_429_at_ms: 0,
            last_success_at_ms: 0,
            last_error_fingerprint: 0,
            window_duration_ms: DEFAULT_WINDOW_DURATION_MS,
            learned_burst: DEFAULT_BURST_CAPACITY,
            window_count: 0,
            in_flight: 0,
            success_streak: 0,
            consecutive_failures: 0,
            consecutive_same_fingerprint: 0,
            state: TargetRateState::Closed,
        }
    }
}

pub fn compute_error_fingerprint(err: &openproxy_types::CoreError) -> u64 {
    let mut hasher = DefaultHasher::new();
    err.http_status().hash(&mut hasher);
    match err {
        openproxy_types::CoreError::UpstreamError { status, body, .. } => {
            status.hash(&mut hasher);
            let mut limit = body.len().min(64);
            while limit > 0 && !body.is_char_boundary(limit) {
                limit -= 1;
            }
            let prefix = &body[..limit];
            prefix.hash(&mut hasher);
        }
        openproxy_types::CoreError::UpstreamConnection(msg) => {
            let mut limit = msg.len().min(64);
            while limit > 0 && !msg.is_char_boundary(limit) {
                limit -= 1;
            }
            let prefix = &msg[..limit];
            prefix.hash(&mut hasher);
        }
        openproxy_types::CoreError::UpstreamTimeout { phase, .. } => {
            phase.hash(&mut hasher);
        }
        _ => {
            std::mem::discriminant(err).hash(&mut hasher);
        }
    }
    hasher.finish()
}

impl TargetPredictiveState {
    pub fn refresh(&mut self, now_ms: u64) {
        self.advance_window_if_expired(now_ms);
        self.try_transition_half_open(now_ms);
        self.apply_memory_decay_if_idle(now_ms);
    }

    fn advance_window_if_expired(&mut self, now_ms: u64) {
        if self.window_start_ms == 0 {
            self.window_start_ms = now_ms;
        }
        if now_ms >= self.window_start_ms + u64::from(self.window_duration_ms) {
            if self.state == TargetRateState::Closed
                && self.window_count > 0
                && self.learned_burst < DEFAULT_BURST_CAPACITY
            {
                self.learned_burst = self
                    .learned_burst
                    .saturating_add(2)
                    .min(DEFAULT_BURST_CAPACITY);
            }
            self.window_start_ms = now_ms;
            self.window_count = 0;
        }
    }

    fn try_transition_half_open(&mut self, now_ms: u64) {
        if self.state == TargetRateState::Open && now_ms >= self.reset_at_ms {
            self.state = TargetRateState::HalfOpen;
            self.window_count = 0;
        }
    }

    fn apply_memory_decay_if_idle(&mut self, now_ms: u64) {
        let is_eligible = self.last_429_at_ms > 0
            && self.state == TargetRateState::Closed
            && self.learned_burst < DEFAULT_BURST_CAPACITY
            && now_ms.saturating_sub(self.last_429_at_ms) > MEMORY_DECAY_IDLE_MS;

        if is_eligible {
            self.learned_burst = self
                .learned_burst
                .saturating_add(5)
                .min(DEFAULT_BURST_CAPACITY);
            self.last_429_at_ms = now_ms;
        }
    }

    pub fn evaluate(&self, now_ms: u64) -> TargetReadiness {
        match self.state {
            TargetRateState::Open => {
                self.saturated_readiness(self.reset_at_ms.saturating_sub(now_ms))
            }
            TargetRateState::HalfOpen if self.in_flight == 0 => TargetReadiness::Probe,
            TargetRateState::HalfOpen => {
                self.saturated_readiness(self.reset_at_ms.saturating_sub(now_ms))
            }
            TargetRateState::Closed if self.window_count >= self.learned_burst => {
                let window_end = self.window_start_ms + u64::from(self.window_duration_ms);
                self.saturated_readiness(window_end.saturating_sub(now_ms))
            }
            TargetRateState::Closed => TargetReadiness::Ready,
        }
    }

    fn saturated_readiness(&self, reset_in_ms: u64) -> TargetReadiness {
        TargetReadiness::Saturated {
            learned_burst: self.learned_burst,
            window_count: self.window_count,
            reset_in_ms,
        }
    }

    pub fn try_acquire(&mut self) -> bool {
        match self.state {
            TargetRateState::Open => false,
            TargetRateState::HalfOpen if self.in_flight == 0 => {
                self.in_flight += 1;
                self.window_count += 1;
                true
            }
            TargetRateState::HalfOpen => false,
            TargetRateState::Closed if self.window_count < self.learned_burst => {
                self.window_count += 1;
                self.in_flight += 1;
                true
            }
            TargetRateState::Closed => false,
        }
    }

    pub fn apply_success(
        &mut self,
        remaining_header: Option<u32>,
        reset_window_secs: Option<u64>,
        now_ms: u64,
    ) {
        self.in_flight = self.in_flight.saturating_sub(1);
        self.last_success_at_ms = now_ms;
        self.consecutive_failures = 0;
        self.consecutive_same_fingerprint = 0;
        self.last_error_fingerprint = 0;

        self.recover_from_half_open();
        self.update_window_duration(reset_window_secs);
        self.calibrate_from_remaining_header(remaining_header);
        self.advance_elastic_streak();
    }

    pub fn should_retry(&self, fingerprint: u64, local_retry_count: u8) -> bool {
        if self.state == TargetRateState::Open {
            return false;
        }
        if local_retry_count >= 1 && fingerprint != 0 && fingerprint == self.last_error_fingerprint
        {
            return false;
        }
        if self.consecutive_failures >= 2 && local_retry_count >= 1 {
            return false;
        }
        true
    }

    pub fn report_upstream_error_with_fingerprint(&mut self, fingerprint: u64, now_ms: u64) {
        self.in_flight = self.in_flight.saturating_sub(1);
        self.success_streak = 0;
        self.state = TargetRateState::Open;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);

        if fingerprint != 0 && fingerprint == self.last_error_fingerprint {
            self.consecutive_same_fingerprint = self.consecutive_same_fingerprint.saturating_add(1);
        } else {
            self.consecutive_same_fingerprint = 1;
            self.last_error_fingerprint = fingerprint;
        }

        let consecutive = self.consecutive_failures.min(8);
        let base_ms: u64 = 15_000;
        let multiplier = 1u64 << consecutive.saturating_sub(1).min(3);
        let penalty_ms = base_ms.saturating_mul(multiplier).min(120_000);
        self.reset_at_ms = now_ms + penalty_ms;
    }

    fn recover_from_half_open(&mut self) {
        if self.state == TargetRateState::HalfOpen {
            self.state = TargetRateState::Closed;
            self.learned_burst = self
                .learned_burst
                .max(self.window_count)
                .max(MIN_RECOVERED_BURST);
        }
    }

    fn update_window_duration(&mut self, reset_window_secs: Option<u64>) {
        if let Some(reset_s) = reset_window_secs {
            self.window_duration_ms = (reset_s.saturating_mul(1000) as u32).max(1000);
        }
    }

    fn calibrate_from_remaining_header(&mut self, remaining_header: Option<u32>) {
        match remaining_header {
            Some(0) => self.learned_burst = self.window_count,
            Some(rem) => {
                let rem_u16 = rem.min(u16::MAX as u32) as u16;
                self.learned_burst = self
                    .learned_burst
                    .max(self.window_count.saturating_add(rem_u16));
            }
            None => {}
        }
    }

    fn advance_elastic_streak(&mut self) {
        self.success_streak += 1;
        if self.success_streak >= PROBE_SUCCESS_THRESHOLD {
            self.learned_burst = self.learned_burst.saturating_add(1);
            self.success_streak = 0;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetReadiness {
    Ready,
    Probe,
    Saturated {
        learned_burst: u16,
        window_count: u16,
        reset_in_ms: u64,
    },
}

#[repr(align(64))]
struct Shard {
    inner: Mutex<HashMap<u64, TargetPredictiveState>>,
}

pub struct PredictiveRateLimiter {
    shards: [Shard; SHARD_COUNT],
}

impl Default for PredictiveRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl PredictiveRateLimiter {
    pub fn new() -> Self {
        Self {
            shards: std::array::from_fn(|_| Shard {
                inner: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64)
    }

    pub fn compute_target_key(target: &ComboTarget) -> u64 {
        Self::compute_key_parts(
            &target.provider_id,
            target.account_id,
            target.model_row_id,
            target.rate_limit_scope,
        )
    }

    pub fn compute_key_parts(
        provider_id: &ProviderId,
        account_id: Option<AccountId>,
        model_row_id: Option<ModelRowId>,
        scope: RateLimitScope,
    ) -> u64 {
        let mut hasher = DefaultHasher::new();
        if let Some(aid) = account_id {
            aid.0.hash(&mut hasher);
            if scope == RateLimitScope::Model {
                model_row_id.unwrap_or(ModelRowId(0)).0.hash(&mut hasher);
            }
        } else {
            provider_id.0.hash(&mut hasher);
            model_row_id.unwrap_or(ModelRowId(0)).0.hash(&mut hasher);
        }
        hasher.finish()
    }

    pub fn compute_key(combo_id: ComboId, target_id: ComboTargetId) -> u64 {
        let mut hasher = DefaultHasher::new();
        combo_id.0.hash(&mut hasher);
        target_id.0.hash(&mut hasher);
        hasher.finish()
    }

    fn shard_for(&self, key: u64) -> &Shard {
        let idx = (key as usize) & (SHARD_COUNT - 1);
        &self.shards[idx]
    }

    /// Evalúa la disponibilidad predictiva del target por clave sin modificar contadores.
    pub fn evaluate_key(&self, key: u64, now_ms: u64) -> TargetReadiness {
        let shard = self.shard_for(key);
        let mut map = shard.inner.lock();
        let state = map.entry(key).or_default();

        state.refresh(now_ms);
        state.evaluate(now_ms)
    }

    /// Evalúa la disponibilidad predictiva del target sin modificar contadores (compatibilidad).
    pub fn evaluate_target(
        &self,
        combo_id: ComboId,
        target_id: ComboTargetId,
        now_ms: u64,
    ) -> TargetReadiness {
        let key = Self::compute_key(combo_id, target_id);
        self.evaluate_key(key, now_ms)
    }

    /// Intenta adquirir el permiso para el target por clave. Retorna true si es admitido
    /// (Ready o Probe) y reserva 1 petición en vuelo y contador de ventana.
    pub fn acquire_key(&self, key: u64, now_ms: u64) -> bool {
        let shard = self.shard_for(key);
        let mut map = shard.inner.lock();
        let state = map.entry(key).or_default();

        state.refresh(now_ms);
        state.try_acquire()
    }

    /// Intenta adquirir el permiso para el target (compatibilidad).
    pub fn acquire_target(&self, combo_id: ComboId, target_id: ComboTargetId, now_ms: u64) -> bool {
        let key = Self::compute_key(combo_id, target_id);
        self.acquire_key(key, now_ms)
    }

    /// Determina si un reintento local en el target está justificado por clave o debe
    /// abortarse inmediatamente (Fast-Fail) para saltar al siguiente target del combo.
    pub fn should_retry_key(
        &self,
        key: u64,
        fingerprint: u64,
        local_retry_count: u8,
        now_ms: u64,
    ) -> bool {
        let shard = self.shard_for(key);
        let mut map = shard.inner.lock();
        let state = map.entry(key).or_default();

        state.refresh(now_ms);
        state.should_retry(fingerprint, local_retry_count)
    }

    /// Determina si un reintento local en el target está justificado (compatibilidad).
    pub fn should_retry(
        &self,
        combo_id: ComboId,
        target_id: ComboTargetId,
        fingerprint: u64,
        local_retry_count: u8,
        now_ms: u64,
    ) -> bool {
        let key = Self::compute_key(combo_id, target_id);
        self.should_retry_key(key, fingerprint, local_retry_count, now_ms)
    }

    /// Decrementa peticiones en vuelo por clave (en caso de cancelación o fallo temprano).
    pub fn release_in_flight_key(&self, key: u64) {
        let shard = self.shard_for(key);
        let mut map = shard.inner.lock();
        if let Some(state) = map.get_mut(&key) {
            state.in_flight = state.in_flight.saturating_sub(1);
        }
    }

    /// Decrementa peticiones en vuelo (compatibilidad).
    pub fn release_in_flight(&self, combo_id: ComboId, target_id: ComboTargetId) {
        let key = Self::compute_key(combo_id, target_id);
        self.release_in_flight_key(key);
    }

    /// Reporta éxito HTTP 200 OK por clave para calibración elástica (Additive Increase).
    pub fn report_success_key(
        &self,
        key: u64,
        remaining_header: Option<u32>,
        reset_window_secs: Option<u64>,
        now_ms: u64,
    ) {
        let shard = self.shard_for(key);
        let mut map = shard.inner.lock();
        let state = map.entry(key).or_default();

        state.apply_success(remaining_header, reset_window_secs, now_ms);
    }

    /// Reporta éxito HTTP 200 OK (compatibilidad).
    pub fn report_success(
        &self,
        combo_id: ComboId,
        target_id: ComboTargetId,
        remaining_header: Option<u32>,
        reset_window_secs: Option<u64>,
        now_ms: u64,
    ) {
        let key = Self::compute_key(combo_id, target_id);
        self.report_success_key(key, remaining_header, reset_window_secs, now_ms);
    }

    /// Reporta HTTP 429 Too Many Requests por clave (Multiplicative Decrease / Burst Cap).
    pub fn report_rate_limited_key(&self, key: u64, retry_after_secs: Option<u64>, now_ms: u64) {
        let shard = self.shard_for(key);
        let mut map = shard.inner.lock();
        let state = map.entry(key).or_default();

        state.in_flight = state.in_flight.saturating_sub(1);
        state.last_429_at_ms = now_ms;
        state.success_streak = 0;
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);

        // El límite real de ráfaga era lo alcanzado antes del 429
        state.learned_burst = std::cmp::max(1, state.window_count.saturating_sub(1));
        state.state = TargetRateState::Open;

        let penalty_ms =
            retry_after_secs.map_or(u64::from(DEFAULT_WINDOW_DURATION_MS), |s| (s * 1000).max(3000));
        state.reset_at_ms = now_ms + penalty_ms;
    }

    /// Reporta HTTP 429 Too Many Requests (compatibilidad).
    pub fn report_rate_limited(
        &self,
        combo_id: ComboId,
        target_id: ComboTargetId,
        retry_after_secs: Option<u64>,
        now_ms: u64,
    ) {
        let key = Self::compute_key(combo_id, target_id);
        self.report_rate_limited_key(key, retry_after_secs, now_ms);
    }

    /// Reporta error upstream genérico por clave (5xx, timeout, connection error).
    pub fn report_upstream_error_key(&self, key: u64, now_ms: u64) {
        self.report_upstream_error_with_fingerprint_key(key, 0, now_ms);
    }

    /// Reporta error upstream genérico (compatibilidad).
    pub fn report_upstream_error(&self, combo_id: ComboId, target_id: ComboTargetId, now_ms: u64) {
        self.report_upstream_error_with_fingerprint(combo_id, target_id, 0, now_ms);
    }

    /// Reporta error upstream con fingerprint por clave para detección de patrones repetitivos.
    pub fn report_upstream_error_with_fingerprint_key(
        &self,
        key: u64,
        fingerprint: u64,
        now_ms: u64,
    ) {
        let shard = self.shard_for(key);
        let mut map = shard.inner.lock();
        let state = map.entry(key).or_default();

        state.report_upstream_error_with_fingerprint(fingerprint, now_ms);
    }

    /// Reporta error upstream con fingerprint para detección de patrones repetitivos (compatibilidad).
    pub fn report_upstream_error_with_fingerprint(
        &self,
        combo_id: ComboId,
        target_id: ComboTargetId,
        fingerprint: u64,
        now_ms: u64,
    ) {
        let key = Self::compute_key(combo_id, target_id);
        self.report_upstream_error_with_fingerprint_key(key, fingerprint, now_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_predictive_learning_and_recovery_cycle() {
        let limiter = PredictiveRateLimiter::new();
        let combo = ComboId(1);
        let target = ComboTargetId(10);
        let mut now = 100_000;

        // Petición 1: éxito
        assert!(limiter.acquire_target(combo, target, now));
        limiter.report_success(combo, target, None, None, now);

        // Petición 2: éxito
        assert!(limiter.acquire_target(combo, target, now));
        limiter.report_success(combo, target, None, None, now);

        // Petición 3: 429 Too Many Requests (el upstream solo toleraba 2)
        assert!(limiter.acquire_target(combo, target, now));
        limiter.report_rate_limited(combo, target, Some(60), now);

        // Ahora el target debe estar Saturado preventivamente
        let readiness = limiter.evaluate_target(combo, target, now);
        match readiness {
            TargetReadiness::Saturated { learned_burst, .. } => {
                assert_eq!(
                    learned_burst, 2,
                    "debe aprender que el límite de ráfaga es 2"
                );
            }
            _ => panic!("debe estar saturado tras 429"),
        }

        // Intentar adquirir de inmediato debe fallar
        assert!(!limiter.acquire_target(combo, target, now));

        // Avanzar el tiempo 61 segundos (vence cooldown)
        now += 61_000;

        // Estado debe pasar a Probe (HalfOpen)
        let probe_readiness = limiter.evaluate_target(combo, target, now);
        assert_eq!(probe_readiness, TargetReadiness::Probe);

        // Se adquiere 1 probe
        assert!(limiter.acquire_target(combo, target, now));
        // Segundo concurrente en HalfOpen no se permite
        assert!(!limiter.acquire_target(combo, target, now));

        // Probe exitoso
        limiter.report_success(combo, target, None, None, now);

        // Vuelve a estar Ready
        assert_eq!(
            limiter.evaluate_target(combo, target, now),
            TargetReadiness::Ready
        );
    }

    #[test]
    fn test_elastic_additive_increase() {
        let limiter = PredictiveRateLimiter::new();
        let combo = ComboId(1);
        let target = ComboTargetId(20);
        let now = 100_000;

        // Petición 1 exitosa, Petición 2 da 429 para aprender capacidad = 1
        assert!(limiter.acquire_target(combo, target, now));
        limiter.report_success(combo, target, None, None, now);

        assert!(limiter.acquire_target(combo, target, now));
        limiter.report_rate_limited(combo, target, Some(10), now);

        let readiness = limiter.evaluate_target(combo, target, now);
        match readiness {
            TargetReadiness::Saturated { learned_burst, .. } => assert_eq!(learned_burst, 1),
            _ => panic!("debe estar saturado con burst 1"),
        }

        // Recuperar a HalfOpen -> Probe exitoso -> Closed (aplica MIN_RECOVERED_BURST = 2)
        let after_cd = now + 15_000;
        assert!(limiter.acquire_target(combo, target, after_cd));
        limiter.report_success(combo, target, None, None, after_cd);

        // 2 éxitos adicionales disparan Additive Increase (+1 por PROBE_SUCCESS_THRESHOLD = 2)
        for i in 0..2 {
            let t = after_cd + 1000 * (i + 1);
            limiter.report_success(combo, target, None, None, t);
        }

        // learned_burst debe haber aumentado de 2 a 3
        let key = PredictiveRateLimiter::compute_key(combo, target);
        let shard = limiter.shard_for(key);
        let map = shard.inner.lock();
        let state = map.get(&key).unwrap();
        assert_eq!(
            state.learned_burst, 3,
            "burst debe haber crecido elásticamente a 3"
        );
    }

    #[test]
    fn test_independent_target_prediction_isolation() {
        let limiter = PredictiveRateLimiter::new();
        let combo = ComboId(1);
        let target_a = ComboTargetId(101);
        let target_b = ComboTargetId(102);
        let now = 100_000;

        // Target A hits 429
        assert!(limiter.acquire_target(combo, target_a, now));
        limiter.report_rate_limited(combo, target_a, Some(60), now);

        // Target A is Saturated
        assert!(!limiter.acquire_target(combo, target_a, now));

        // Target B is completely independent and Ready
        assert_eq!(
            limiter.evaluate_target(combo, target_b, now),
            TargetReadiness::Ready
        );
        assert!(limiter.acquire_target(combo, target_b, now));
    }

    #[test]
    fn test_chain_skipping_sequential_decision() {
        let limiter = PredictiveRateLimiter::new();
        let combo = ComboId(1);
        let target_1 = ComboTargetId(201);
        let target_2 = ComboTargetId(202);
        let target_3 = ComboTargetId(203);
        let now = 100_000;

        // Target 1 toleró 2 peticiones y luego dio 429
        assert!(limiter.acquire_target(combo, target_1, now));
        limiter.report_success(combo, target_1, None, None, now);
        assert!(limiter.acquire_target(combo, target_1, now));
        limiter.report_rate_limited(combo, target_1, Some(60), now);

        // Target 2 está virgen (Ready)
        // Target 3 está virgen (Ready)

        // En una nueva petición:
        // Evaluamos Target 1 -> Saturated (debe saltarse si hay alternativas)
        let t1_ready = limiter.evaluate_target(combo, target_1, now);
        assert!(matches!(
            t1_ready,
            TargetReadiness::Saturated {
                learned_burst: 1,
                ..
            }
        ));

        // Target 2 está listo y atiende la petición
        assert_eq!(
            limiter.evaluate_target(combo, target_2, now),
            TargetReadiness::Ready
        );
        assert!(limiter.acquire_target(combo, target_2, now));
        limiter.report_success(combo, target_2, None, None, now);

        // Target 3 sigue listo
        assert_eq!(
            limiter.evaluate_target(combo, target_3, now),
            TargetReadiness::Ready
        );
    }

    #[test]
    fn test_upstream_error_blocks_and_recovers() {
        let limiter = PredictiveRateLimiter::new();
        let combo = ComboId(1);
        let target = ComboTargetId(300);
        let now = 100_000;

        // Un error upstream marca Open con penalty escalado
        assert!(limiter.acquire_target(combo, target, now));
        limiter.report_upstream_error(combo, target, now);

        // Target debe estar Saturated
        let r = limiter.evaluate_target(combo, target, now);
        assert!(
            matches!(r, TargetReadiness::Saturated { .. }),
            "upstream error debe saturar"
        );

        // learned_burst NO fue reducido (sigue en default 1000)
        let key = PredictiveRateLimiter::compute_key(combo, target);
        let shard = limiter.shard_for(key);
        let reset_at = {
            let map = shard.inner.lock();
            let state = map.get(&key).unwrap();
            assert_eq!(
                state.learned_burst, DEFAULT_BURST_CAPACITY,
                "upstream error no reduce burst"
            );
            state.reset_at_ms
        };

        // Tras expirar penalty, debe pasar a HalfOpen/Probe
        let after = reset_at + 1;
        let r2 = limiter.evaluate_target(combo, target, after);
        assert_eq!(
            r2,
            TargetReadiness::Probe,
            "debe pasar a Probe tras penalty"
        );

        // Probe exitoso recupera
        assert!(limiter.acquire_target(combo, target, after));
        limiter.report_success(combo, target, None, None, after);
        assert_eq!(
            limiter.evaluate_target(combo, target, after),
            TargetReadiness::Ready
        );
    }

    #[test]
    fn test_upstream_error_escalating_penalty() {
        let limiter = PredictiveRateLimiter::new();
        let combo = ComboId(1);
        let target = ComboTargetId(400);
        let mut now = 100_000;

        // Primer error
        assert!(limiter.acquire_target(combo, target, now));
        limiter.report_upstream_error(combo, target, now);

        let key = PredictiveRateLimiter::compute_key(combo, target);
        let shard = limiter.shard_for(key);
        let reset_1 = {
            let map = shard.inner.lock();
            map.get(&key).unwrap().reset_at_ms
        };
        let penalty_1 = reset_1 - now;

        // Avanzar justo después del primer penalty y dar segundo error
        now = reset_1 + 1;
        assert!(limiter.acquire_target(combo, target, now));
        limiter.report_upstream_error(combo, target, now);

        let reset_2 = {
            let map = shard.inner.lock();
            map.get(&key).unwrap().reset_at_ms
        };
        let penalty_2 = reset_2 - now;

        // El segundo penalty debe ser >= que el primero (escalado exponencial)
        assert!(
            penalty_2 >= penalty_1,
            "penalty debe escalar: {penalty_2} >= {penalty_1}"
        );

        // Y debe estar capeado a <= 120s
        assert!(
            penalty_2 <= 120_000,
            "penalty debe estar capeado: {penalty_2} <= 120000"
        );
    }

    #[test]
    fn test_fingerprint_fast_fail_repeating_error() {
        let limiter = PredictiveRateLimiter::new();
        let combo = ComboId(1);
        let target = ComboTargetId(500);
        let now = 100_000;

        let err1 = openproxy_types::CoreError::upstream_error(
            500,
            "opencode-zen",
            "muse-spark",
            "{\"type\":\"error\",\"error\":{\"type\":\"error\",\"message\":\"Internal server error\"}}",
            false,
        );
        let fp1 = compute_error_fingerprint(&err1);

        // Primer fallo: se reporta
        assert!(limiter.acquire_target(combo, target, now));
        limiter.report_upstream_error_with_fingerprint(combo, target, fp1, now);

        // Si se evalúa retry local con el MISMO fingerprint en intento local 1, debe dar Fast-Fail (false)
        let can_retry = limiter.should_retry(combo, target, fp1, 1, now);
        assert!(
            !can_retry,
            "debe hacer fast-fail ante el mismo fingerprint repetitivo"
        );
    }

    #[test]
    fn test_success_resets_failure_fingerprints() {
        let limiter = PredictiveRateLimiter::new();
        let combo = ComboId(1);
        let target = ComboTargetId(600);
        let mut now = 100_000;

        let err = openproxy_types::CoreError::UpstreamConnection("connect error".to_string());
        let fp = compute_error_fingerprint(&err);

        assert!(limiter.acquire_target(combo, target, now));
        limiter.report_upstream_error_with_fingerprint(combo, target, fp, now);

        // Expirar cooldown
        now += 30_000;
        assert!(limiter.acquire_target(combo, target, now));
        // Éxito
        limiter.report_success(combo, target, None, None, now);

        // Verificar que un nuevo error ahora sí permitiría retry normal en intento 0
        assert!(limiter.should_retry(combo, target, fp, 0, now));
    }

    #[test]
    fn test_account_and_model_key_isolation() {
        let limiter = PredictiveRateLimiter::new();
        let prov = ProviderId("openai".to_string());
        let acc1 = Some(AccountId(10));
        let acc2 = Some(AccountId(20));
        let model1 = Some(ModelRowId(100));
        let model2 = Some(ModelRowId(200));

        let k_acc1 =
            PredictiveRateLimiter::compute_key_parts(&prov, acc1, model1, RateLimitScope::Account);
        let k_acc2 =
            PredictiveRateLimiter::compute_key_parts(&prov, acc2, model1, RateLimitScope::Account);
        assert_ne!(
            k_acc1, k_acc2,
            "Diferentes cuentas deben tener claves distintas"
        );

        let k_model1 =
            PredictiveRateLimiter::compute_key_parts(&prov, acc1, model1, RateLimitScope::Model);
        let k_model2 =
            PredictiveRateLimiter::compute_key_parts(&prov, acc1, model2, RateLimitScope::Model);
        assert_ne!(
            k_model1, k_model2,
            "Diferentes modelos en scope Model deben tener claves distintas"
        );

        let now = 100_000;
        assert!(limiter.acquire_key(k_acc1, now));
        limiter.report_rate_limited_key(k_acc1, Some(60), now);

        assert_eq!(
            limiter.evaluate_key(k_acc1, now),
            TargetReadiness::Saturated {
                learned_burst: 1,
                window_count: 1,
                reset_in_ms: 60_000,
            }
        );
        assert_eq!(limiter.evaluate_key(k_acc2, now), TargetReadiness::Ready);
    }

    #[test]
    fn test_unbound_account_model_isolation() {
        let prov = ProviderId("anthropic".to_string());
        let model1 = Some(ModelRowId(10));
        let model2 = Some(ModelRowId(20));

        let k1 =
            PredictiveRateLimiter::compute_key_parts(&prov, None, model1, RateLimitScope::Account);
        let k2 =
            PredictiveRateLimiter::compute_key_parts(&prov, None, model2, RateLimitScope::Account);
        assert_ne!(
            k1, k2,
            "Modelos distintos sin account_id deben generar claves aisladas"
        );

        let k1_model_scope =
            PredictiveRateLimiter::compute_key_parts(&prov, None, model1, RateLimitScope::Model);
        let k2_model_scope =
            PredictiveRateLimiter::compute_key_parts(&prov, None, model2, RateLimitScope::Model);
        assert_ne!(
            k1_model_scope, k2_model_scope,
            "Modelos distintos en scope Model deben generar claves aisladas"
        );
        assert_eq!(
            k1, k1_model_scope,
            "Sin account_id, la clave es idéntica independientemente del scope"
        );
    }

    #[test]
    fn test_window_advance_additive_increase() {
        let mut state = TargetPredictiveState {
            state: TargetRateState::Closed,
            learned_burst: 10,
            window_count: 5,
            window_start_ms: 100_000,
            window_duration_ms: 60_000,
            ..Default::default()
        };

        // Ventana no expirada: no debe incrementar
        state.refresh(120_000);
        assert_eq!(state.learned_burst, 10);
        assert_eq!(state.window_count, 5);

        // Ventana expirada con window_count > 0: debe incrementar +2
        state.refresh(160_000);
        assert_eq!(state.learned_burst, 12);
        assert_eq!(state.window_count, 0);
        assert_eq!(state.window_start_ms, 160_000);

        // Otra ventana expirada con window_count == 0: no debe incrementar
        state.refresh(230_000);
        assert_eq!(state.learned_burst, 12);
        assert_eq!(state.window_count, 0);
    }

    #[test]
    fn test_recover_from_half_open_min_recovered_burst() {
        let mut state = TargetPredictiveState {
            state: TargetRateState::HalfOpen,
            learned_burst: 1,
            window_count: 1,
            ..Default::default()
        };

        state.apply_success(None, None, 100_000);
        assert_eq!(state.state, TargetRateState::Closed);
        assert_eq!(
            state.learned_burst, MIN_RECOVERED_BURST,
            "Recuperarse de HalfOpen debe garantizar al menos MIN_RECOVERED_BURST"
        );
    }
}
