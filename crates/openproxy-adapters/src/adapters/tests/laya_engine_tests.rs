use crate::adapters::laya_engine::{
    confidence_from_probs, execute_decision, init, is_available, is_model_installed,
    render_options, shutdown,
};
use openproxy_types::systemone::{SystemOneQuestion, SystemOneQuestionType, SystemOneRequest};
use std::{collections::BTreeMap, sync::Arc};

#[test]
fn test_confidence_from_probs_boundary_cases() {
    assert_eq!(confidence_from_probs(&[], 0), 1.0);
    assert_eq!(confidence_from_probs(&[1.0], 1), 1.0);

    // Uniform distribution with k=2: maximum entropy -> confidence = 0.0
    let conf_uniform_2 = confidence_from_probs(&[0.5, 0.5], 2);
    assert!(conf_uniform_2 < 1e-4, "Expected ~0.0, got {conf_uniform_2}");

    // Certain distribution with k=2: zero entropy -> confidence = 1.0
    let conf_certain_2 = confidence_from_probs(&[1.0, 0.0], 2);
    assert!(
        (conf_certain_2 - 1.0).abs() < 1e-4,
        "Expected ~1.0, got {conf_certain_2}"
    );

    // Uniform distribution with k=4: maximum entropy -> confidence = 0.0
    let conf_uniform_4 = confidence_from_probs(&[0.25, 0.25, 0.25, 0.25], 4);
    assert!(conf_uniform_4 < 1e-4, "Expected ~0.0, got {conf_uniform_4}");

    // Highly confident distribution with k=4 -> confidence > 0.85
    let conf_skewed_4 = confidence_from_probs(&[0.97, 0.01, 0.01, 0.01], 4);
    assert!(conf_skewed_4 > 0.85, "Expected >0.85, got {conf_skewed_4}");
}

#[test]
fn test_render_options_all_types() {
    // 1. Choice with criteria object
    let mut criteria_obj = serde_json::Map::new();
    criteria_obj.insert("a".into(), serde_json::Value::String("option A".into()));
    criteria_obj.insert("b".into(), serde_json::Value::Null);
    criteria_obj.insert("c".into(), serde_json::Value::String(String::new()));
    let q_choice = SystemOneQuestion {
        question_type: SystemOneQuestionType::Choice,
        instructions: "Choose".into(),
        criteria: Some(serde_json::Value::Object(criteria_obj)),
        options: None,
    };
    let opts = render_options(&q_choice);
    assert_eq!(opts.len(), 3);
    assert!(opts.iter().any(|(k, v)| k == "a" && v == "a: option A"));
    assert!(opts.iter().any(|(k, v)| k == "b" && v == "b"));
    assert!(opts.iter().any(|(k, v)| k == "c" && v == "c"));

    // 2. Choice fallback to options array
    let q_opts = SystemOneQuestion {
        question_type: SystemOneQuestionType::Choice,
        instructions: "Choose".into(),
        criteria: None,
        options: Some(vec!["opt1".into(), "opt2".into()]),
    };
    let opts_fallback = render_options(&q_opts);
    assert_eq!(opts_fallback.len(), 2);
    assert_eq!(opts_fallback[0], ("opt1".into(), "opt1".into()));

    // 3. Score with criteria array
    let q_score = SystemOneQuestion {
        question_type: SystemOneQuestionType::Score,
        instructions: "Rate".into(),
        criteria: Some(serde_json::json!(["low", "medium", "high"])),
        options: None,
    };
    let score_opts = render_options(&q_score);
    assert_eq!(score_opts.len(), 3);
    assert_eq!(score_opts[0], ("0".into(), "level 0: low".into()));
    assert_eq!(score_opts[1], ("1".into(), "level 1: medium".into()));
    assert_eq!(score_opts[2], ("2".into(), "level 2: high".into()));

    // 4. Noul with custom criteria
    let q_noul = SystemOneQuestion {
        question_type: SystemOneQuestionType::Noul,
        instructions: "Verify".into(),
        criteria: Some(serde_json::json!({
            "false": "statement is false",
            "true": "statement is true"
        })),
        options: None,
    };
    let noul_opts = render_options(&q_noul);
    assert_eq!(noul_opts.len(), 2);
    assert_eq!(
        noul_opts[0],
        ("false".into(), "false: statement is false".into())
    );
    assert_eq!(
        noul_opts[1],
        ("true".into(), "true: statement is true".into())
    );

    // 5. Noul with default criteria
    let q_noul_def = SystemOneQuestion {
        question_type: SystemOneQuestionType::Noul,
        instructions: "Verify".into(),
        criteria: None,
        options: None,
    };
    let noul_def_opts = render_options(&q_noul_def);
    assert_eq!(noul_def_opts.len(), 2);
    assert_eq!(
        noul_def_opts[0],
        (
            "false".into(),
            "false: no, the statement does not hold".into()
        )
    );
    assert_eq!(
        noul_def_opts[1],
        ("true".into(), "true: yes, the statement holds".into())
    );
}

static TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn has_model() -> bool {
    is_model_installed()
}

#[test]
#[ignore = "requires local Laya ONNX model weights"]
fn test_laya_engine_lifecycle_and_idempotency() {
    let _lock = TEST_MUTEX.lock().unwrap();
    if !has_model() {
        return;
    }

    shutdown();
    assert!(!is_available());

    // 1. Initial init
    init(None, None, None, Some(4)).expect("first init succeeds");
    assert!(is_available());

    // 2. Second init is idempotent no-op
    init(None, None, None, Some(4)).expect("second init is no-op");
    assert!(is_available());

    // 3. Shutdown frees resources
    shutdown();
    assert!(!is_available());

    // 4. Second shutdown is safe no-op
    shutdown();
    assert!(!is_available());

    // 5. Re-init after shutdown
    init(None, None, None, Some(4)).expect("re-init succeeds");
    assert!(is_available());

    shutdown();
    assert!(!is_available());
}

#[test]
#[ignore = "requires local Laya ONNX model weights"]
fn test_laya_engine_multi_question_types_inference() {
    let _lock = TEST_MUTEX.lock().unwrap();
    if !has_model() {
        return;
    }

    init(None, None, None, Some(4)).expect("init");

    let mut questions = BTreeMap::new();
    questions.insert(
        "topic".to_string(),
        SystemOneQuestion {
            question_type: SystemOneQuestionType::Choice,
            instructions: "Categorize prompt".into(),
            criteria: Some(serde_json::json!({
                "coding": "Code, debugging, programming",
                "creative": "Writing stories, poetry",
                "factual": "Facts, encyclopedia, history"
            })),
            options: None,
        },
    );
    questions.insert(
        "urgency".to_string(),
        SystemOneQuestion {
            question_type: SystemOneQuestionType::Score,
            instructions: "Rate urgency".into(),
            criteria: Some(serde_json::json!(["low", "medium", "high"])),
            options: None,
        },
    );
    questions.insert(
        "is_code".to_string(),
        SystemOneQuestion {
            question_type: SystemOneQuestionType::Noul,
            instructions: "Does the user ask for code?".into(),
            criteria: None,
            options: None,
        },
    );

    let req = SystemOneRequest {
        state: serde_json::json!("Write a Python function to parse JSON with sqlite3"),
        model: Some("laya-multilingual".into()),
        questions,
    };

    let resp = execute_decision(&req).expect("inference succeeds");
    assert_eq!(resp.model.as_deref(), Some("laya-multilingual"));

    // Choice validation
    let topic = resp.answers.get("topic").expect("topic answer");
    assert_eq!(topic.question_type, SystemOneQuestionType::Choice);
    assert_eq!(topic.choice.as_deref(), Some("coding"));
    assert!(topic.confidence.unwrap_or(0.0) > 0.0);
    assert!(
        topic
            .probabilities
            .as_ref()
            .is_some_and(|p| p.contains_key("coding"))
    );

    // Score validation
    let urgency = resp.answers.get("urgency").expect("urgency answer");
    assert_eq!(urgency.question_type, SystemOneQuestionType::Score);
    let score = urgency.score.expect("score present");
    assert!((0.0..=2.0).contains(&score));

    // Noul validation
    let is_code = resp.answers.get("is_code").expect("is_code answer");
    assert_eq!(is_code.question_type, SystemOneQuestionType::Noul);
    let noul = is_code.noul.expect("noul prob present");
    assert!((0.0..=1.0).contains(&noul));
    assert!(
        noul > 0.5,
        "Expected high probability for code query, got {noul}"
    );

    // Usage tokens validation
    assert!(resp.usage.is_some_and(|u| u.input_tokens > 0));

    shutdown();
}

#[test]
#[ignore = "requires local Laya ONNX model weights"]
fn test_laya_engine_multithreaded_concurrency() {
    let _lock = TEST_MUTEX.lock().unwrap();
    if !has_model() {
        return;
    }

    init(None, None, None, Some(4)).expect("init");

    let req = Arc::new(SystemOneRequest {
        state: serde_json::json!("Fix a deadlock in Rust parking_lot Mutex"),
        model: Some("laya-multilingual".into()),
        questions: {
            let mut q = BTreeMap::new();
            q.insert(
                "topic".to_string(),
                SystemOneQuestion {
                    question_type: SystemOneQuestionType::Choice,
                    instructions: "Classify topic".into(),
                    criteria: Some(serde_json::json!({
                        "coding": "Programming and debugging",
                        "creative": "Creative writing"
                    })),
                    options: None,
                },
            );
            q
        },
    });

    let mut handles = Vec::new();
    for _ in 0..8 {
        let r = Arc::clone(&req);
        handles.push(std::thread::spawn(move || {
            let resp = execute_decision(&r).expect("inference should succeed in thread");
            let ans = resp.answers.get("topic").expect("answer topic");
            ans.choice.clone()
        }));
    }

    for h in handles {
        let choice = h.join().expect("thread should join");
        assert_eq!(choice.as_deref(), Some("coding"));
    }

    shutdown();
}

#[test]
#[ignore = "requires local Laya ONNX model weights"]
fn test_laya_engine_multibyte_utf8_and_long_prompt() {
    let _lock = TEST_MUTEX.lock().unwrap();
    if !has_model() {
        return;
    }

    init(None, None, None, Some(4)).expect("init");

    // Complex multibyte with emojis, Japanese, Spanish accents, symbols
    let complex_text = format!(
        "¿Cómo depurar un deadlock en Rust con un servidor SQLite? 🚀 🦀 日本語のテスト {}\n{}",
        "A".repeat(5000), // Ensure truncation > max_len works safely
        "Por favor escribe un script funcional."
    );

    let req = SystemOneRequest {
        state: serde_json::json!(complex_text),
        model: Some("laya-multilingual".into()),
        questions: {
            let mut q = BTreeMap::new();
            q.insert(
                "topic".to_string(),
                SystemOneQuestion {
                    question_type: SystemOneQuestionType::Choice,
                    instructions: "Clasificación de tema: ¿es código o literatura?".into(),
                    criteria: Some(serde_json::json!({
                        "coding": "Desarrollo de software y código",
                        "creative": "Narrativa y literatura"
                    })),
                    options: None,
                },
            );
            q
        },
    };

    let resp = execute_decision(&req).expect("multibyte inference succeeds without panic");
    let ans = resp.answers.get("topic").expect("answer");
    assert_eq!(ans.choice.as_deref(), Some("coding"));

    shutdown();
}
