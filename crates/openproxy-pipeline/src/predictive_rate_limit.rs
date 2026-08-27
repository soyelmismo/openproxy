use openproxy_types::ids::{ComboId, ComboTargetId};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

const SHARD_COUNT: usize = 256;
const DEFAULT_BURST_CAPACITY: u32 = 1000;
const DEFAULT_WINDOW_DURATION_MS: u64 = 60_000;
const PROBE_SUCCESS_THRESHOLD: u32 = 5;
const MEMORY_DECAY_IDLE_MS: u64 = 15 * 60 * 1000; // 15 min sin 429s -> decay

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetRateState {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug, Clone)]
pub struct TargetPredictiveState {
    pub state: TargetRateState,
    pub learned_burst: u32,
    pub window_count: u32,
    pub in_flight: u32,
    pub success_streak: u32,
    pub window_start_ms: u64,
    pub window_duration_ms: u64,
    pub reset_at_ms: u64,
    pub last_429_at_ms: u64,
    pub last_success_at_ms: u64,
}

impl Default for TargetPredictiveState {
    fn default() -> Self {
        Self {
            state: TargetRateState::Closed,
            learned_burst: DEFAULT_BURST_CAPACITY,
            window_count: 0,
            in_flight: 0,
            success_streak: 0,
            window_start_ms: 0,
            window_duration_ms: DEFAULT_WINDOW_DURATION_MS,
            reset_at_ms: 0,
            last_429_at_ms: 0,
            last_success_at_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetReadiness {
    Ready,
    Probe,
    Saturated {
        learned_burst: u32,
        window_count: u32,
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

    fn compute_key(combo_id: ComboId, target_id: ComboTargetId) -> u64 {
        let mut hasher = DefaultHasher::new();
        combo_id.0.hash(&mut hasher);
        target_id.0.hash(&mut hasher);
        hasher.finish()
    }

    fn shard_for(&self, key: u64) -> &Shard {
        let idx = (key as usize) & (SHARD_COUNT - 1);
        &self.shards[idx]
    }

    /// Evalúa la disponibilidad predictiva del target sin modificar contadores.
    pub fn evaluate_target(
        &self,
        combo_id: ComboId,
        target_id: ComboTargetId,
        now_ms: u64,
    ) -> TargetReadiness {
        let key = Self::compute_key(combo_id, target_id);
        let shard = self.shard_for(key);
        let mut map = shard.inner.lock();
        let state = map.entry(key).or_default();

        Self::refresh_state(state, now_ms);

        match state.state {
            TargetRateState::Open => TargetReadiness::Saturated {
                learned_burst: state.learned_burst,
                window_count: state.window_count,
                reset_in_ms: state.reset_at_ms.saturating_sub(now_ms),
            },
            TargetRateState::HalfOpen => {
                if state.in_flight == 0 {
                    TargetReadiness::Probe
                } else {
                    TargetReadiness::Saturated {
                        learned_burst: state.learned_burst,
                        window_count: state.window_count,
                        reset_in_ms: state.reset_at_ms.saturating_sub(now_ms),
                    }
                }
            }
            TargetRateState::Closed => {
                if state.window_count >= state.learned_burst {
                    let window_end = state.window_start_ms + state.window_duration_ms;
                    TargetReadiness::Saturated {
                        learned_burst: state.learned_burst,
                        window_count: state.window_count,
                        reset_in_ms: window_end.saturating_sub(now_ms),
                    }
                } else {
                    TargetReadiness::Ready
                }
            }
        }
    }

    /// Intenta adquirir el permiso para el target. Retorna true si es admitido
    /// (Ready o Probe) y reserva 1 petición en vuelo y contador de ventana.
    pub fn acquire_target(
        &self,
        combo_id: ComboId,
        target_id: ComboTargetId,
        now_ms: u64,
    ) -> bool {
        let key = Self::compute_key(combo_id, target_id);
        let shard = self.shard_for(key);
        let mut map = shard.inner.lock();
        let state = map.entry(key).or_default();

        Self::refresh_state(state, now_ms);

        match state.state {
            TargetRateState::Open => false,
            TargetRateState::HalfOpen => {
                if state.in_flight == 0 {
                    state.in_flight += 1;
                    state.window_count += 1;
                    true
                } else {
                    false
                }
            }
            TargetRateState::Closed => {
                if state.window_count >= state.learned_burst {
                    false
                } else {
                    state.window_count += 1;
                    state.in_flight += 1;
                    true
                }
            }
        }
    }

    /// Decrementa peticiones en vuelo (en caso de cancelación o fallo temprano).
    pub fn release_in_flight(&self, combo_id: ComboId, target_id: ComboTargetId) {
        let key = Self::compute_key(combo_id, target_id);
        let shard = self.shard_for(key);
        let mut map = shard.inner.lock();
        if let Some(state) = map.get_mut(&key) {
            state.in_flight = state.in_flight.saturating_sub(1);
        }
    }

    /// Reporta éxito HTTP 200 OK para calibración elástica (Additive Increase).
    pub fn report_success(
        &self,
        combo_id: ComboId,
        target_id: ComboTargetId,
        remaining_header: Option<u32>,
        reset_window_secs: Option<u64>,
        now_ms: u64,
    ) {
        let key = Self::compute_key(combo_id, target_id);
        let shard = self.shard_for(key);
        let mut map = shard.inner.lock();
        let state = map.entry(key).or_default();

        state.in_flight = state.in_flight.saturating_sub(1);
        state.last_success_at_ms = now_ms;

        if state.state == TargetRateState::HalfOpen {
            state.state = TargetRateState::Closed;
            state.learned_burst = state.learned_burst.max(state.window_count);
        }

        if let Some(reset_s) = reset_window_secs {
            state.window_duration_ms = (reset_s * 1000).max(1000);
        }

        // Si el upstream reporta remaining explícito, calibramos directamente
        if let Some(rem) = remaining_header {
            if rem == 0 {
                state.learned_burst = state.window_count;
            } else {
                state.learned_burst = state.learned_burst.max(state.window_count + rem);
            }
        }

        // Expansión elástica: N éxitos seguidos incrementan la capacidad aprendida
        state.success_streak += 1;
        if state.success_streak >= PROBE_SUCCESS_THRESHOLD {
            state.learned_burst = state.learned_burst.saturating_add(1);
            state.success_streak = 0;
        }
    }

    /// Reporta HTTP 429 Too Many Requests (Multiplicative Decrease / Burst Cap).
    pub fn report_rate_limited(
        &self,
        combo_id: ComboId,
        target_id: ComboTargetId,
        retry_after_secs: Option<u64>,
        now_ms: u64,
    ) {
        let key = Self::compute_key(combo_id, target_id);
        let shard = self.shard_for(key);
        let mut map = shard.inner.lock();
        let state = map.entry(key).or_default();

        state.in_flight = state.in_flight.saturating_sub(1);
        state.last_429_at_ms = now_ms;
        state.success_streak = 0;

        // El límite real de ráfaga era lo alcanzado antes del 429
        state.learned_burst = std::cmp::max(1, state.window_count.saturating_sub(1));
        state.state = TargetRateState::Open;

        let penalty_ms = retry_after_secs
            .map_or(DEFAULT_WINDOW_DURATION_MS, |s| (s * 1000).max(3000));
        state.reset_at_ms = now_ms + penalty_ms;
    }

    /// Mantenimiento temporal y decaimiento de penalización
    fn refresh_state(state: &mut TargetPredictiveState, now_ms: u64) {
        // 1. Inicialización de ventana
        if state.window_start_ms == 0 {
            state.window_start_ms = now_ms;
        }

        // 2. Expiración de ventana deslizante
        if now_ms >= state.window_start_ms + state.window_duration_ms {
            state.window_start_ms = now_ms;
            state.window_count = 0;
            if state.state == TargetRateState::Closed {
                // Ventana renovada
            }
        }

        // 3. Expiración de cooldown (Open -> HalfOpen)
        if state.state == TargetRateState::Open && now_ms >= state.reset_at_ms {
            state.state = TargetRateState::HalfOpen;
            state.window_count = 0;
        }

        // 4. Decaimiento de memoria por inactividad prolongada sin 429s
        if state.last_429_at_ms > 0
            && now_ms.saturating_sub(state.last_429_at_ms) > MEMORY_DECAY_IDLE_MS
            && state.state == TargetRateState::Closed
            && state.learned_burst < DEFAULT_BURST_CAPACITY
        {
            state.learned_burst = state.learned_burst.saturating_add(5).min(DEFAULT_BURST_CAPACITY);
            state.last_429_at_ms = now_ms; // Escalón de decaimiento
        }
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
                assert_eq!(learned_burst, 2, "debe aprender que el límite de ráfaga es 2");
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
        assert_eq!(limiter.evaluate_target(combo, target, now), TargetReadiness::Ready);
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

        // Recuperar a HalfOpen -> Probe exitoso -> Closed
        let after_cd = now + 15_000;
        assert!(limiter.acquire_target(combo, target, after_cd));
        limiter.report_success(combo, target, None, None, after_cd);

        // 5 éxitos consecutivos deben disparar Additive Increase
        for i in 0..5 {
            let t = after_cd + 1000 * (i + 1);
            limiter.report_success(combo, target, None, None, t);
        }

        // learned_burst debe haber aumentado a 2
        let key = PredictiveRateLimiter::compute_key(combo, target);
        let shard = limiter.shard_for(key);
        let map = shard.inner.lock();
        let state = map.get(&key).unwrap();
        assert_eq!(state.learned_burst, 2, "burst debe haber crecido elásticamente a 2");
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
        assert_eq!(limiter.evaluate_target(combo, target_b, now), TargetReadiness::Ready);
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
        assert!(matches!(t1_ready, TargetReadiness::Saturated { learned_burst: 1, .. }));

        // Target 2 está listo y atiende la petición
        assert_eq!(limiter.evaluate_target(combo, target_2, now), TargetReadiness::Ready);
        assert!(limiter.acquire_target(combo, target_2, now));
        limiter.report_success(combo, target_2, None, None, now);

        // Target 3 sigue listo
        assert_eq!(limiter.evaluate_target(combo, target_3, now), TargetReadiness::Ready);
    }
}
