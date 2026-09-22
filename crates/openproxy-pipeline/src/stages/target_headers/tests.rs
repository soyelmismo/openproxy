use super::*;
    use openproxy_adapters::spoofer::OPENCODE_UA;

    #[test]
    fn test_propagate_cline_headers() {
        let mut headers = vec![
            ("User-Agent".into(), "Cline/4.1.3".into()),
            ("x-title".into(), "Cline".into()),
            ("Authorization".into(), "Bearer secret".into()),
        ];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("x-cline-task-id".into(), "task-999".into());
        req_headers.insert("cline-mode".into(), "act".into());
        req_headers.insert("x-platform".into(), "Visual Studio Code".into());
        req_headers.insert("x-client-version".into(), "4.2.0".into());
        req_headers.insert("x-is-multiroot".into(), "true".into());
        req_headers.insert("authorization".into(), "override-hack".into());

        propagate_cline_headers(&mut headers, &req_headers);

        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("x-cline-task-id"), Some("task-999"));
        assert_eq!(find("cline-mode"), Some("act"));
        assert_eq!(find("x-platform"), Some("Visual Studio Code"));
        assert_eq!(find("x-client-version"), Some("4.2.0"));
        assert_eq!(find("x-is-multiroot"), Some("true"));
        assert_eq!(find("Authorization"), Some("Bearer secret"));
    }

    #[test]
    fn test_propagate_kilocode_headers() {
        let mut headers = vec![
            ("User-Agent".into(), "Kilo-Code/4.108.0".into()),
            ("x-title".into(), "Kilo Code".into()),
            ("Authorization".into(), "Bearer kl-secret".into()),
        ];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("x-kilocode-taskid".into(), "task-kilo-123".into());
        req_headers.insert("x-kilocode-feature".into(), "openclaw".into());
        req_headers.insert("kilocode-org".into(), "kilo-team".into());
        req_headers.insert("x-client-version".into(), "5.0.1".into());
        req_headers.insert("authorization".into(), "override-hack".into());

        propagate_kilocode_headers(&mut headers, &req_headers);

        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("x-kilocode-taskid"), Some("task-kilo-123"));
        assert_eq!(find("x-kilocode-feature"), Some("openclaw"));
        assert_eq!(find("kilocode-org"), Some("kilo-team"));
        assert_eq!(find("x-client-version"), Some("5.0.1"));
        assert_eq!(find("Authorization"), Some("Bearer kl-secret"));
    }

    #[test]
    fn test_propagate_codex_headers() {
        let mut headers = vec![
            (
                "User-Agent".into(),
                "codex-cli/0.144.0 (Windows 10.0.26200; x64)".into(),
            ),
            ("origin".into(), "https://chatgpt.com".into()),
            ("originator".into(), "codex_cli_rs".into()),
            ("Authorization".into(), "Bearer codex-tok".into()),
        ];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("x-codex-session".into(), "ses-codex-1".into());
        req_headers.insert("chatgpt-account-id".into(), "ws-team-456".into());
        req_headers.insert("codex-subaction".into(), "lint".into());
        req_headers.insert("originator".into(), "codex_exec".into());
        req_headers.insert("version".into(), "0.150.0".into());
        req_headers.insert("authorization".into(), "override-hack".into());

        propagate_codex_headers(&mut headers, &req_headers);

        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("x-codex-session"), Some("ses-codex-1"));
        assert_eq!(find("chatgpt-account-id"), Some("ws-team-456"));
        assert_eq!(find("codex-subaction"), Some("lint"));
        assert_eq!(find("originator"), Some("codex_exec"));
        assert_eq!(find("version"), Some("0.150.0"));
        assert_eq!(find("Authorization"), Some("Bearer codex-tok"));
    }

    #[test]
    fn test_propagate_minimax_headers() {
        let mut headers = vec![
            ("User-Agent".into(), "MiniMaxAgent".into()),
            ("Anthropic-Version".into(), "2023-06-01".into()),
            ("x-api-key".into(), "secret".into()),
        ];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("anthropic-beta".into(), "prompt-caching-2024-07-31".into());
        req_headers.insert("x-mavis-agent-id".into(), "custom-agent".into());
        req_headers.insert("x-minimax-feature".into(), "v2".into());
        req_headers.insert("x-conversation-id".into(), "conv-abc-789".into());
        req_headers.insert("authorization".into(), "override-hack".into());

        propagate_minimax_headers(&mut headers, &req_headers);

        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("anthropic-beta"), Some("prompt-caching-2024-07-31"));
        assert_eq!(find("x-mavis-agent-id"), Some("custom-agent"));
        assert_eq!(find("x-minimax-feature"), Some("v2"));
        assert_eq!(find("x-mavis-session-id"), Some("session_conv-abc-789"));
        assert_eq!(find("x-api-key"), Some("secret"));
        assert_eq!(find("authorization"), None);
    }

    #[test]
    fn test_propagate_kiro_headers() {
        let mut headers = vec![
            ("Content-Type".into(), "application/json".into()),
            (
                "x-amz-user-agent".into(),
                "aws-sdk-js/3.0.0 kiro/0.1".into(),
            ),
            ("Authorization".into(), "Bearer kiro-tok".into()),
        ];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("tokentype".into(), "API_KEY".into());
        req_headers.insert("x-kiro-profile".into(), "enterprise-1".into());
        req_headers.insert("kiro-task".into(), "analyze".into());
        req_headers.insert("x-conversation-id".into(), "conv-kiro-999".into());
        req_headers.insert("anthropic-beta".into(), "prompt-caching-2024-07-31".into());
        req_headers.insert("x-amzn-bedrock-cache-control".into(), "enable".into());
        req_headers.insert("authorization".into(), "hack".into());

        propagate_kiro_headers(&mut headers, &req_headers);

        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("tokentype"), Some("API_KEY"));
        assert_eq!(find("x-kiro-profile"), Some("enterprise-1"));
        assert_eq!(find("kiro-task"), Some("analyze"));
        assert_eq!(find("x-conversation-id"), Some("conv-kiro-999"));
        assert_eq!(find("anthropic-beta"), Some("prompt-caching-2024-07-31"));
        assert_eq!(find("x-amzn-bedrock-cache-control"), Some("enable"));
        assert_eq!(find("Authorization"), Some("Bearer kiro-tok"));
    }

    #[test]
    fn test_propagate_commandcode_headers() {
        let mut headers = vec![
            ("Content-Type".into(), "application/json".into()),
            ("user-agent".into(), "cli".into()),
            ("x-command-code-version".into(), "1.54.0".into()),
            ("Authorization".into(), "Bearer cc-tok".into()),
        ];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("x-command-code-task".into(), "build".into());
        req_headers.insert("x-project-slug".into(), "my-project".into());
        req_headers.insert("x-cli-environment".into(), "staging".into());
        req_headers.insert("x-taste-learning".into(), "false".into());
        req_headers.insert("x-conversation-id".into(), "conv-cc-789".into());
        req_headers.insert("x-command-code-version".into(), "1.60.0".into());
        req_headers.insert("authorization".into(), "hack".into());

        propagate_commandcode_headers(&mut headers, &req_headers);

        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("x-command-code-task"), Some("build"));
        assert_eq!(find("x-project-slug"), Some("my-project"));
        assert_eq!(find("x-cli-environment"), Some("staging"));
        assert_eq!(find("x-taste-learning"), Some("false"));
        assert_eq!(find("x-conversation-id"), Some("conv-cc-789"));
        assert_eq!(find("x-command-code-version"), Some("1.60.0"));
        assert_eq!(find("Authorization"), Some("Bearer cc-tok"));
    }

    #[test]
    fn test_propagate_opencode_headers_default() {
        let _guard = openproxy_adapters::spoofer::OPENCODE_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        openproxy_adapters::spoofer::reset_dynamic_opencode_overrides();

        let mut headers = vec![("Content-Type".into(), "application/json".into())];
        let req_headers = std::collections::BTreeMap::new();
        let openai_req = openproxy_types::OpenAIRequest {
            model: "big-pickle".into(),
            messages: vec![],
            stream: false,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            top_k: None,
            user: None,
            extra: serde_json::Map::new(),
        };

        propagate_opencode_headers(&mut headers, &req_headers, &openai_req);

        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("User-Agent"), Some(OPENCODE_UA));
        assert_eq!(find("x-opencode-client"), Some("cli"));
        assert_eq!(find("x-opencode-project"), Some("global"));
        assert!(find("x-opencode-session").unwrap().starts_with("ses_"));
        assert!(find("x-opencode-request").unwrap().starts_with("msg_"));
    }

    #[test]
    fn test_propagate_opencode_headers_custom_user_agent() {
        let _guard = openproxy_adapters::spoofer::OPENCODE_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        openproxy_adapters::spoofer::reset_dynamic_opencode_overrides();

        let mut headers = vec![("User-Agent".into(), OPENCODE_UA.into())];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("user-agent".into(), "opencode/1.19.0".into());
        let openai_req = openproxy_types::OpenAIRequest {
            model: "big-pickle".into(),
            messages: vec![],
            stream: false,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            top_k: None,
            user: None,
            extra: serde_json::Map::new(),
        };

        propagate_opencode_headers(&mut headers, &req_headers, &openai_req);
        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(find("User-Agent"), Some("opencode/1.19.0"));
    }

    #[test]
    fn test_propagate_opencode_dynamic_headers_and_extensions() {
        use openproxy_adapters::spoofer::{
            reset_dynamic_opencode_overrides, set_dynamic_opencode_version,
        };

        let _guard = openproxy_adapters::spoofer::OPENCODE_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        reset_dynamic_opencode_overrides();
        set_dynamic_opencode_version("1.30.0");

        let mut headers = vec![("Content-Type".into(), "application/json".into())];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("x-opencode-custom-flag".into(), "speed-mode".into());
        req_headers.insert("x-opencode-debug".into(), "1".into());

        let openai_req = openproxy_types::OpenAIRequest {
            model: "big-pickle".into(),
            messages: vec![],
            stream: false,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            top_k: None,
            user: None,
            extra: serde_json::Map::new(),
        };

        propagate_opencode_headers(&mut headers, &req_headers, &openai_req);

        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("User-Agent"), Some("opencode/1.30.0"));
        assert_eq!(find("x-opencode-custom-flag"), Some("speed-mode"));
        assert_eq!(find("x-opencode-debug"), Some("1"));

        reset_dynamic_opencode_overrides();
    }

    #[test]
    fn test_propagate_antigravity_headers() {
        let mut headers = vec![
            ("User-Agent".into(), "Antigravity/4.3.0".into()),
            ("x-client-name".into(), "antigravity".into()),
            ("x-client-version".into(), "4.3.0".into()),
        ];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("x-cloudaicompanion-trace-id".into(), "0x123abc".into());
        req_headers.insert("x-antigravity-custom".into(), "custom-val".into());
        req_headers.insert("x-goog-new-feature".into(), "enabled".into());
        // Prohibited headers must be skipped
        req_headers.insert("x-goog-api-client".into(), "malicious-sdk".into());
        req_headers.insert("x-client-version".into(), "hack".into());

        propagate_antigravity_headers(&mut headers, &req_headers);

        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("x-cloudaicompanion-trace-id"), Some("0x123abc"));
        assert_eq!(find("x-antigravity-custom"), Some("custom-val"));
        assert_eq!(find("x-goog-new-feature"), Some("enabled"));
        assert_eq!(find("x-goog-api-client"), None);
        assert_eq!(find("x-client-version"), Some("4.3.0"));
    }

    #[test]
    fn test_propagate_provider_target_headers_dispatch() {
        let openai_req = openproxy_types::OpenAIRequest {
            model: "test-model".into(),
            messages: vec![],
            stream: false,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            top_k: None,
            user: None,
            extra: serde_json::Map::new(),
        };

        // Codex with workspace id
        let mut headers = vec![("User-Agent".into(), "Codex/1.0".into())];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("originator".into(), "codex_cli_rs".into());
        propagate_provider_target_headers(
            &mut headers,
            "codex-default",
            "openai",
            &req_headers,
            &openai_req,
            Some("ws-123"),
        );
        let find = |h: &[(String, String)], k: &str| {
            h.iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
                .map(String::from)
        };
        assert_eq!(find(&headers, "originator"), Some("codex_cli_rs".into()));
        assert_eq!(find(&headers, "chatgpt-account-id"), Some("ws-123".into()));

        // CommandCode alias
        let mut cmd_headers = vec![("User-Agent".into(), "CmdCode/1.0".into())];
        let mut cmd_req_headers = std::collections::BTreeMap::new();
        cmd_req_headers.insert("x-commandcode-session-id".into(), "cmd-sess-99".into());
        propagate_provider_target_headers(
            &mut cmd_headers,
            "cmd",
            "commandcode-adapter",
            &cmd_req_headers,
            &openai_req,
            None,
        );
        assert_eq!(
            find(&cmd_headers, "x-conversation-id"),
            Some("cmd-sess-99".into())
        );

        // CodeBuddy dispatch
        let mut cb_headers = vec![("User-Agent".into(), "CLI/2.156.0 CodeBuddy/2.156.0".into())];
        let mut cb_req_headers = std::collections::BTreeMap::new();
        cb_req_headers.insert("x-codebuddy-task-id".into(), "cb-task-99".into());
        propagate_provider_target_headers(
            &mut cb_headers,
            "codebuddy",
            "codebuddy",
            &cb_req_headers,
            &openai_req,
            None,
        );
        assert_eq!(
            find(&cb_headers, "x-codebuddy-task-id"),
            Some("cb-task-99".into())
        );
    }

    #[test]
    fn test_propagate_codebuddy_headers() {
        let mut headers = vec![
            ("x-ide-type".into(), "CLI".into()),
            ("x-product".into(), "SaaS".into()),
            ("Authorization".into(), "Bearer secret".into()),
        ];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("x-codebuddy-task-id".into(), "task-cb-42".into());
        req_headers.insert("x-ide-version".into(), "2.160.0".into());
        req_headers.insert("x-agent-intent".into(), "ask".into());
        req_headers.insert("codebuddy-session".into(), "sess-1".into());
        req_headers.insert("user-agent".into(), "CodeBuddy/2.170.0 (Darwin; x64)".into());
        req_headers.insert("x-codebuddy-session-id".into(), "cb-sess-99".into());

        propagate_codebuddy_headers(&mut headers, &req_headers);

        let find = |key: &str| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("x-codebuddy-task-id"), Some("task-cb-42"));
        assert_eq!(find("x-ide-version"), Some("2.160.0"));
        assert_eq!(find("x-agent-intent"), Some("ask"));
        assert_eq!(find("codebuddy-session"), Some("sess-1"));
        assert_eq!(find("Authorization"), Some("Bearer secret"));
        assert_eq!(find("User-Agent"), Some("CodeBuddy/2.170.0 (Darwin; x64)"));
        assert_eq!(find("x-conversation-id"), Some("cb-sess-99"));
    }
