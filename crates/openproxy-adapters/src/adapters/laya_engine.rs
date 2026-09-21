use openproxy_types::{
    error::CoreError,
    systemone::{
        SystemOneAnswer, SystemOneQuestion, SystemOneQuestionType, SystemOneRequest,
        SystemOneResponse, SystemOneUsage,
    },
};
use parking_lot::RwLock;
use std::{
    collections::BTreeMap,
    ffi::{CStr, CString},
    sync::Arc,
};
use tokenizers::Tokenizer;

#[repr(C)]
struct LayaSessionOpaque {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn laya_session_create(
        onnx_lib_path: *const std::ffi::c_char,
        model_path: *const std::ffi::c_char,
        num_threads: std::ffi::c_int,
        err_buf: *mut std::ffi::c_char,
        err_buf_len: usize,
    ) -> *mut LayaSessionOpaque;

    fn laya_session_destroy(session: *mut LayaSessionOpaque);

    fn laya_session_run(
        session: *mut LayaSessionOpaque,
        batch_size: i64,
        seq_len: i64,
        max_markers: i64,
        input_ids: *const i64,
        attention_mask: *const i64,
        marker_pos: *const i64,
        marker_mask: *const u8,
        qtype: *const i64,
        out_logits: *mut f32,
        err_buf: *mut std::ffi::c_char,
        err_buf_len: usize,
    ) -> std::ffi::c_int;
}

struct SafeSession(*mut LayaSessionOpaque);
unsafe impl Send for SafeSession {}
unsafe impl Sync for SafeSession {}

impl Drop for SafeSession {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { laya_session_destroy(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct LayaConfig {
    #[serde(default = "default_max_len")]
    pub max_len: usize,
    #[serde(default = "default_head_max_len")]
    pub head_max_len: usize,
    #[serde(default = "default_temperature")]
    pub temperature: Vec<f32>,
    #[serde(default)]
    pub temperature_by_options: std::collections::HashMap<String, f32>,
}

fn default_max_len() -> usize {
    512
}
fn default_head_max_len() -> usize {
    192
}
fn default_temperature() -> Vec<f32> {
    vec![1.0, 1.0, 1.0]
}

impl Default for LayaConfig {
    fn default() -> Self {
        Self {
            max_len: default_max_len(),
            head_max_len: default_head_max_len(),
            temperature: default_temperature(),
            temperature_by_options: std::collections::HashMap::new(),
        }
    }
}

pub struct LayaEngine {
    session: SafeSession,
    tokenizer: Tokenizer,
    pad_id: u32,
    cls_id: u32,
    sep_id: u32,
    mask_id: u32,
    config: LayaConfig,
}

static INSTANCE: RwLock<Option<Arc<LayaEngine>>> = RwLock::new(None);

pub fn is_available() -> bool {
    INSTANCE.read().is_some()
}

pub fn shutdown() {
    let mut guard = INSTANCE.write();
    if guard.is_some() {
        tracing::info!("Unloading Laya ONNX internal engine (releasing memory)");
        *guard = None;
    }
}

fn resolve_candidate_path(
    explicit: Option<&str>,
    env_var: &str,
    filename: &str,
    extra_subpaths: &[&str],
    dev_fallbacks: &[&str],
) -> String {
    if let Some(p) = explicit.filter(|s| !s.is_empty()) {
        return p.to_string();
    }
    if let Some(p) = std::env::var(env_var).ok().filter(|s| !s.is_empty()) {
        return p;
    }

    let mut candidates = Vec::new();

    // 1. ~/.openproxy/models/laya/<subpath>
    if let Ok(home) = std::env::var("HOME") {
        let base = format!("{home}/.openproxy/models/laya");
        candidates.push(format!("{base}/{filename}"));
        for sub in extra_subpaths {
            candidates.push(format!("{base}/{sub}"));
        }
    }

    // 2. ./models/laya/<subpath>
    candidates.push(format!("./models/laya/{filename}"));
    for sub in extra_subpaths {
        candidates.push(format!("./models/laya/{sub}"));
    }

    // 3. Dev fallbacks
    for cand in dev_fallbacks {
        candidates.push((*cand).to_string());
    }

    for cand in &candidates {
        if std::path::Path::new(cand).exists() {
            return cand.clone();
        }
    }

    // Default canonical path if none exist yet
    if let Ok(home) = std::env::var("HOME") {
        format!("{home}/.openproxy/models/laya/{filename}")
    } else {
        format!("./models/laya/{filename}")
    }
}

pub fn resolve_model_path(opt: Option<&str>) -> String {
    resolve_candidate_path(
        opt,
        "OPENPROXY_LAYA_MODEL",
        "model.onnx",
        &[
            "model-fp32/model.onnx",
            "model-int8/model.onnx",
            "model-fp32.onnx",
            "model-int8.onnx",
        ],
        &[],
    )
}

pub fn resolve_tokenizer_path(opt: Option<&str>) -> String {
    resolve_candidate_path(
        opt,
        "OPENPROXY_LAYA_TOKENIZER",
        "tokenizer.json",
        &["tokenizer/tokenizer.json"],
        &[],
    )
}

pub fn resolve_config_path(opt: Option<&str>) -> String {
    resolve_candidate_path(
        opt,
        "OPENPROXY_LAYA_CONFIG",
        "rl_agent_config.json",
        &["onnx_config.json"],
        &[],
    )
}

pub fn is_model_installed() -> bool {
    let p = resolve_model_path(None);
    std::path::Path::new(&p).exists()
}

pub fn init(
    model_path_opt: Option<&str>,
    tokenizer_path_opt: Option<&str>,
    config_path_opt: Option<&str>,
    num_threads_opt: Option<usize>,
) -> Result<(), CoreError> {
    let mut guard = INSTANCE.write();
    if guard.is_some() {
        return Ok(());
    }

    let model_path = resolve_model_path(model_path_opt);
    let tokenizer_path = resolve_tokenizer_path(tokenizer_path_opt);
    let config_path = resolve_config_path(config_path_opt);

    let num_threads = num_threads_opt
        .or_else(|| {
            std::env::var("OPENPROXY_LAYA_THREADS")
                .ok()
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get().clamp(1, 4)));

    tracing::info!(
        model = %model_path,
        tokenizer = %tokenizer_path,
        threads = num_threads,
        "Initializing Laya internal C FFI engine"
    );

    let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
        CoreError::Internal(format!(
            "Failed to load tokenizer from {tokenizer_path}: {e}"
        ))
    })?;

    let pad_id = tokenizer.token_to_id("<pad>").unwrap_or(0);
    let cls_id = tokenizer
        .token_to_id("<bos>")
        .or_else(|| tokenizer.token_to_id("[CLS]"))
        .unwrap_or(2);
    let sep_id = tokenizer
        .token_to_id("<eos>")
        .or_else(|| tokenizer.token_to_id("[SEP]"))
        .unwrap_or(1);
    let mask_id = tokenizer
        .token_to_id("<mask >")
        .or_else(|| tokenizer.token_to_id("<mask >"))
        .unwrap_or(4);

    let config: LayaConfig = if let Ok(content) = std::fs::read_to_string(&config_path) {
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        LayaConfig::default()
    };

    let c_model_path = CString::new(model_path)
        .map_err(|e| CoreError::Validation(format!("Invalid model path: {e}")))?;

    let mut err_buf = vec![0u8; 1024];
    let session_ptr = unsafe {
        laya_session_create(
            std::ptr::null(),
            c_model_path.as_ptr(),
            num_threads as std::ffi::c_int,
            err_buf.as_mut_ptr().cast::<std::ffi::c_char>(),
            err_buf.len(),
        )
    };

    if session_ptr.is_null() {
        let err_msg = unsafe { CStr::from_ptr(err_buf.as_ptr().cast::<std::ffi::c_char>()) }
            .to_string_lossy()
            .into_owned();
        return Err(CoreError::Internal(format!(
            "Failed to initialize Laya ONNX session: {err_msg}"
        )));
    }

    let engine = LayaEngine {
        session: SafeSession(session_ptr),
        tokenizer,
        pad_id,
        cls_id,
        sep_id,
        mask_id,
        config,
    };

    *guard = Some(Arc::new(engine));
    tracing::info!("Laya ONNX internal engine ready for in-process inference");
    Ok(())
}

fn render_criterion(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

pub(crate) fn render_options(q: &SystemOneQuestion) -> Vec<(String, String)> {
    match q.question_type {
        SystemOneQuestionType::Choice => {
            if let Some(ref crit) = q.criteria
                && let Some(obj) = crit.as_object()
            {
                return obj
                    .iter()
                    .map(|(k, v)| {
                        let text = if v.is_null() || v.as_str().is_some_and(|s| s.is_empty()) {
                            k.clone()
                        } else {
                            format!("{k}: {}", render_criterion(v))
                        };
                        (k.clone(), text)
                    })
                    .collect();
            }
            if let Some(ref opts) = q.options {
                return opts.iter().map(|o| (o.clone(), o.clone())).collect();
            }
            vec![]
        }
        SystemOneQuestionType::Score => {
            if let Some(ref crit) = q.criteria
                && let Some(arr) = crit.as_array()
            {
                return arr
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        (
                            format!("{i}"),
                            format!("level {i}: {}", render_criterion(c)),
                        )
                    })
                    .collect();
            }
            vec![]
        }
        SystemOneQuestionType::Noul => {
            let (f_txt, t_txt) = if let Some(ref crit) = q.criteria {
                let f = crit.get("false").map(render_criterion);
                let t = crit.get("true").map(render_criterion);
                (
                    f.unwrap_or_else(|| "no, the statement does not hold".into()),
                    t.unwrap_or_else(|| "yes, the statement holds".into()),
                )
            } else {
                (
                    "no, the statement does not hold".into(),
                    "yes, the statement holds".into(),
                )
            };
            vec![
                ("false".into(), format!("false: {f_txt}")),
                ("true".into(), format!("true: {t_txt}")),
            ]
        }
    }
}

struct PreparedItem {
    ids: Vec<i64>,
    markers: Vec<i64>,
    qtype: i64,
    option_keys: Vec<String>,
}

impl LayaEngine {
    fn tokenize_ids(&self, text: &str, limit: Option<usize>) -> Result<Vec<i64>, CoreError> {
        let clean = text.replace("<mask >", " ");
        let enc = self
            .tokenizer
            .encode(clean.as_str(), false)
            .map_err(|e| CoreError::Internal(format!("Tokenizer encode error: {e}")))?;
        let iter = enc.get_ids().iter().map(|&x| x as i64);
        Ok(match limit {
            Some(n) => iter.take(n).collect(),
            None => iter.collect(),
        })
    }

    fn build_sequence(
        &self,
        state_str: &str,
        q: &SystemOneQuestion,
    ) -> Result<PreparedItem, CoreError> {
        let opts = render_options(q);
        let qtype_code = match q.question_type {
            SystemOneQuestionType::Choice => 0i64,
            SystemOneQuestionType::Score => 1i64,
            SystemOneQuestionType::Noul => 2i64,
        };

        let head_prompt = format!("{} question: {}", q.question_type.as_str(), q.instructions);
        let mut head_ids = self.tokenize_ids(&head_prompt, None)?;

        let mut opt_ids: Vec<Vec<i64>> = Vec::with_capacity(opts.len());
        for (_, opt_text) in &opts {
            let ids = self.tokenize_ids(&format!(" {opt_text}"), Some(48))?;
            let mut full = Vec::with_capacity(ids.len() + 1);
            full.push(self.mask_id as i64);
            full.extend(ids);
            opt_ids.push(full);
        }

        let total_opt_len: usize = opt_ids.iter().map(|o| o.len()).sum();
        let mut opt_budget = self.config.head_max_len.saturating_sub(total_opt_len);
        if opt_budget < 16 && !opt_ids.is_empty() {
            let per = ((self.config.head_max_len.saturating_sub(16)) / opt_ids.len()).max(4);
            for o in &mut opt_ids {
                o.truncate(per);
            }
            let new_opt_len: usize = opt_ids.iter().map(|o| o.len()).sum();
            opt_budget = self.config.head_max_len.saturating_sub(new_opt_len);
        }

        head_ids.truncate(opt_budget.max(8));

        let mut ids: Vec<i64> = Vec::with_capacity(self.config.max_len);
        ids.push(self.cls_id as i64);
        ids.extend(head_ids);
        ids.push(self.sep_id as i64);

        let mut markers: Vec<i64> = Vec::with_capacity(opt_ids.len());
        for o in opt_ids {
            markers.push(ids.len() as i64);
            ids.extend(o);
        }
        ids.push(self.sep_id as i64);

        let room = self.config.max_len.saturating_sub(ids.len() + 1);
        let st_ids = self.tokenize_ids(state_str, Some(room))?;

        ids.extend(st_ids);
        ids.push(self.sep_id as i64);
        ids.truncate(self.config.max_len);

        markers.retain(|&m| (m as usize) < self.config.max_len);

        Ok(PreparedItem {
            ids,
            markers,
            qtype: qtype_code,
            option_keys: opts.into_iter().map(|(k, _)| k).collect(),
        })
    }

    pub fn classify(&self, req: &SystemOneRequest) -> Result<SystemOneResponse, CoreError> {
        let q_keys: Vec<String> = req.questions.keys().cloned().collect();
        if q_keys.is_empty() {
            return Ok(SystemOneResponse {
                model: Some("laya-inprocess".into()),
                answers: BTreeMap::new(),
                usage: Some(SystemOneUsage::default()),
            });
        }

        let state_str = match &req.state {
            serde_json::Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        };

        let mut items = Vec::with_capacity(q_keys.len());
        for key in &q_keys {
            let q = &req.questions[key];
            let item = self.build_sequence(&state_str, q)?;
            items.push(item);
        }

        let batch_size = items.len() as i64;
        let max_seq_len = items.iter().map(|it| it.ids.len()).max().unwrap_or(1) as i64;
        let max_markers = items.iter().map(|it| it.markers.len()).max().unwrap_or(1) as i64;

        let total_seq_elems = (batch_size * max_seq_len) as usize;
        let total_marker_elems = (batch_size * max_markers) as usize;

        let mut input_ids = vec![self.pad_id as i64; total_seq_elems];
        let mut attention_mask = vec![0i64; total_seq_elems];
        let mut marker_pos = vec![0i64; total_marker_elems];
        let mut marker_mask = vec![0u8; total_marker_elems];
        let mut qtype = vec![0i64; batch_size as usize];

        let mut total_tokens = 0u64;

        for (i, it) in items.iter().enumerate() {
            let seq_offset = i * (max_seq_len as usize);
            for (j, &id) in it.ids.iter().enumerate() {
                input_ids[seq_offset + j] = id;
                attention_mask[seq_offset + j] = 1;
                total_tokens += 1;
            }

            let marker_offset = i * (max_markers as usize);
            for (j, &m) in it.markers.iter().enumerate() {
                marker_pos[marker_offset + j] = m;
                marker_mask[marker_offset + j] = 1;
            }

            qtype[i] = it.qtype;
        }

        let mut out_logits = vec![0.0f32; total_marker_elems];
        let mut err_buf = vec![0u8; 1024];

        let rc = unsafe {
            laya_session_run(
                self.session.0,
                batch_size,
                max_seq_len,
                max_markers,
                input_ids.as_ptr(),
                attention_mask.as_ptr(),
                marker_pos.as_ptr(),
                marker_mask.as_ptr(),
                qtype.as_ptr(),
                out_logits.as_mut_ptr(),
                err_buf.as_mut_ptr().cast::<std::ffi::c_char>(),
                err_buf.len(),
            )
        };

        if rc != 0 {
            let err_msg = unsafe { CStr::from_ptr(err_buf.as_ptr().cast::<std::ffi::c_char>()) }
                .to_string_lossy()
                .into_owned();
            return Err(CoreError::Internal(format!(
                "Laya ONNX inference failed (rc={rc}): {err_msg}"
            )));
        }

        let mut answers = BTreeMap::new();

        for (r, key) in q_keys.into_iter().enumerate() {
            let q = &req.questions[&key];
            let item = &items[r];
            let k = item.markers.len();
            if k == 0 {
                continue;
            }

            let marker_offset = r * (max_markers as usize);
            let raw_logits = &out_logits[marker_offset..marker_offset + k];

            let temp = self.get_temperature(item.qtype, k);
            let inv_temp = 1.0 / temp.max(1e-3);

            let mut scaled: Vec<f32> = raw_logits.iter().map(|&x| x * inv_temp).collect();
            let max_logit = scaled.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let mut sum_exp = 0.0f32;
            for x in &mut scaled {
                *x = (*x - max_logit).exp();
                sum_exp += *x;
            }
            let probs: Vec<f64> = scaled
                .iter()
                .map(|&x| ((x / sum_exp) as f64).clamp(0.0, 1.0))
                .collect();

            let conf = confidence_from_probs(&probs, k);

            match q.question_type {
                SystemOneQuestionType::Choice => {
                    let best_idx = probs
                        .iter()
                        .enumerate()
                        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
                        .map_or(0, |(idx, _)| idx);

                    let choice_name = item
                        .option_keys
                        .get(best_idx)
                        .cloned()
                        .unwrap_or_else(|| format!("{best_idx}"));

                    let mut prob_map = BTreeMap::new();
                    for (idx, opt_name) in item.option_keys.iter().enumerate() {
                        prob_map.insert(opt_name.clone(), round_4(probs[idx]));
                    }

                    answers.insert(
                        key,
                        SystemOneAnswer {
                            question_type: SystemOneQuestionType::Choice,
                            choice: Some(choice_name),
                            confidence: Some(round_4(conf)),
                            probabilities: Some(prob_map),
                            ..Default::default()
                        },
                    );
                }
                SystemOneQuestionType::Score => {
                    let score: f64 = probs
                        .iter()
                        .enumerate()
                        .map(|(idx, &p)| (idx as f64) * p)
                        .sum();

                    answers.insert(
                        key,
                        SystemOneAnswer {
                            question_type: SystemOneQuestionType::Score,
                            score: Some(round_4(score)),
                            confidence: Some(round_4(conf)),
                            ..Default::default()
                        },
                    );
                }
                SystemOneQuestionType::Noul => {
                    let noul_prob = probs.get(1).copied().unwrap_or(0.0);
                    answers.insert(
                        key,
                        SystemOneAnswer {
                            question_type: SystemOneQuestionType::Noul,
                            noul: Some(round_4(noul_prob)),
                            confidence: Some(round_4(conf)),
                            ..Default::default()
                        },
                    );
                }
            }
        }

        let resp_model = req
            .model
            .clone()
            .unwrap_or_else(|| "laya-multilingual".into());
        Ok(SystemOneResponse {
            model: Some(resp_model),
            answers,
            usage: Some(SystemOneUsage {
                input_tokens: total_tokens,
                output_tokens: 0,
                total_tokens: Some(total_tokens),
            }),
        })
    }

    fn get_temperature(&self, qtype: i64, k: usize) -> f32 {
        let type_str = match qtype {
            0 => "choice",
            1 => "score",
            _ => "noul",
        };
        let size_str = if k <= 2 {
            "2"
        } else if k <= 5 {
            "3-5"
        } else if k <= 10 {
            "6-10"
        } else {
            "11+"
        };
        let bucket = format!("{type_str}:{size_str}");
        if let Some(&t) = self.config.temperature_by_options.get(&bucket) {
            return t;
        }
        self.config
            .temperature
            .get(qtype as usize)
            .copied()
            .unwrap_or(1.0)
    }
}

pub(crate) fn confidence_from_probs(probs: &[f64], k: usize) -> f64 {
    if k < 2 {
        return 1.0;
    }
    let mut ent = 0.0;
    for &p in probs.iter().take(k) {
        let p_clamped = p.clamp(1e-12, 1.0);
        ent -= p * p_clamped.ln();
    }
    let norm = (k as f64).ln();
    (1.0 - (ent / norm)).clamp(0.0, 1.0)
}

fn round_4(val: f64) -> f64 {
    (val * 10000.0).round() / 10000.0
}

pub fn execute_decision(req: &SystemOneRequest) -> Result<SystemOneResponse, CoreError> {
    let guard = INSTANCE.read();
    let engine = guard.as_ref().ok_or_else(|| {
        CoreError::Internal("Laya internal engine is not initialized or active".into())
    })?;
    engine.classify(req)
}

pub fn spawn_init_background() {
    tokio::task::spawn_blocking(|| {
        if let Err(e) = init(None, None, None, None) {
            tracing::warn!(error = %e, "Failed to initialize Laya ONNX engine in background");
        }
    });
}
