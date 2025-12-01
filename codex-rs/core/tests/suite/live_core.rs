#[ignore]
#[tokio::test]
async fn live_core_gemini_complex_edit() -> anyhow::Result<()> {
    // This test constructs a real Session with Gemini provider and runs a multi-turn conversation.
    // It avoids the TUI layer and uses codex-core directly.

    let _api_key = match std::env::var("GEMINI_API_KEY") {
        Ok(key) => key,
        Err(_) => {
            eprintln!("skipping live_core_gemini_complex_edit – GEMINI_API_KEY not set");
            return Ok(());
        }
    };
    println!("GEMINI_API_KEY length: {}", _api_key.len());

    use codex_core::AuthManager;
    use codex_core::auth::AuthCredentialsStoreMode;
    use codex_core::codex::Codex;
    use codex_core::config::Config;
    use codex_core::config::ConfigOverrides;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::InitialHistory;
    use codex_protocol::protocol::Op;
    use codex_protocol::protocol::SessionSource;
    use std::sync::Arc;
    use tempfile::tempdir;

    let dir = tempdir()?;
    let math_py = dir.path().join("math.py");
    std::fs::write(
        &math_py,
        "def add(a, b):\n    return a + b\n\ndef sub(a, b):\n    return a - b\n",
    )?;

    // Load default config
    // We set CODEX_HOME to ensure it uses our temp dir
    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }
    // We use a blank ConfigOverrides and load from a temporary "home" to ensure defaults.
    let mut config = Config::load_with_cli_overrides(vec![], ConfigOverrides::default()).await?;

    config.cwd = dir.path().to_path_buf();
    // Create a specific provider configuration for this test to ensure env_key is set correctly
    use codex_core::ModelProviderInfo;
    use codex_core::WireApi;

    let gemini_provider = ModelProviderInfo {
        name: "Gemini Test".to_string(),
        base_url: Some("https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-flash:streamGenerateContent".to_string()),
        env_key: Some("GEMINI_API_KEY".to_string()),
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: WireApi::Gemini,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        requires_openai_auth: false,
    };

    config
        .model_providers
        .insert("gemini-test".to_string(), gemini_provider);
    config.model_provider_id = "gemini-test".to_string();

    config.model = "gemini-1.5-flash".to_string();
    config.approval_policy = codex_protocol::protocol::AskForApproval::Never;
    config.sandbox_policy = codex_protocol::protocol::SandboxPolicy::DangerFullAccess;

    let auth_manager = Arc::new(AuthManager::new(
        config.codex_home.clone(),
        true,                           // enable_codex_api_key_env
        AuthCredentialsStoreMode::Auto, // Assuming Auto is safe or default
    ));

    let spawn_ok = Codex::spawn(
        config,
        auth_manager,
        InitialHistory::New,
        SessionSource::Exec,
    )
    .await?;

    let codex = spawn_ok.codex;

    // Turn 1: Ask for edit
    let turn_id = codex.submit(Op::UserTurn {
        items: vec![codex_protocol::user_input::UserInput::Text {
            text: "Edit math.py: Rename the 'add' function to 'sum_vals' and change it to return 'a + b + 0'. Also add a docstring to 'sub' saying 'Subtracts stuff'.".to_string()
        }],
        cwd: dir.path().to_path_buf(),
        approval_policy: codex_protocol::protocol::AskForApproval::Never,
        sandbox_policy: codex_protocol::protocol::SandboxPolicy::DangerFullAccess,
        model: "gemini-2.0-flash-thinking-exp".to_string(),
        effort: None,
        summary: codex_protocol::config_types::ReasoningSummary::None,
        final_output_json_schema: None,
    }).await?;

    // Wait for completion
    let mut saw_diff = false;
    loop {
        let event = codex.next_event().await?;
        println!("Event: {:?}", event.msg);
        match event.msg {
            // TaskComplete event contains id of the submission? No, TaskCompleteEvent struct.
            // Wait, Event wrapper has `id` which is submission id.
            EventMsg::TaskComplete(_completed) => {
                assert_eq!(event.id, turn_id);
                break;
            }
            EventMsg::TurnAborted(aborted) => {
                panic!("Turn aborted: {:?}", aborted);
            }
            EventMsg::TurnDiff(_diff) => {
                // We expect some diff
                saw_diff = true;
            }
            _ => {}
        }
    }

    assert!(saw_diff, "Expected to see a TurnDiff event");

    // Verify file content
    let contents = std::fs::read_to_string(&math_py)?;
    assert!(
        contents.contains("def sum_vals(a, b):"),
        "Should have renamed function. Content:\n{}",
        contents
    );
    assert!(
        contents.contains("return a + b + 0"),
        "Should have changed return. Content:\n{}",
        contents
    );
    assert!(
        contents.contains("Subtracts stuff"),
        "Should have added docstring. Content:\n{}",
        contents
    );

    Ok(())
}

#[ignore]
#[tokio::test]
async fn live_core_gemini_apply_patch_context() -> anyhow::Result<()> {
    // This test verifies that Gemini can successfully use apply_patch with multiple @@ context lines.

    let _api_key = match std::env::var("GEMINI_API_KEY") {
        Ok(key) => key,
        Err(_) => {
            eprintln!("skipping live_core_gemini_apply_patch_context – GEMINI_API_KEY not set");
            return Ok(());
        }
    };

    use codex_core::AuthManager;
    use codex_core::auth::AuthCredentialsStoreMode;
    use codex_core::codex::Codex;
    use codex_core::config::Config;
    use codex_core::config::ConfigOverrides;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::InitialHistory;
    use codex_protocol::protocol::Op;
    use codex_protocol::protocol::SessionSource;
    use std::sync::Arc;
    use tempfile::tempdir;

    let dir = tempdir()?;
    let context_py = dir.path().join("context.py");
    // Create a file with ambiguous code blocks that might force the model to use multiple @@ lines
    std::fs::write(
        &context_py,
        r#"class Calculator:
    def process(self):
        print("processing")
        self.log("start")
        # ... complicated logic ...
        self.log("end")

class Logger:
    def process(self):
        print("processing")
        self.log("start")
        # ... complicated logic ...
        self.log("end")
"#,
    )?;

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }
    let mut config = Config::load_with_cli_overrides(vec![], ConfigOverrides::default()).await?;

    config.cwd = dir.path().to_path_buf();

    use codex_core::ModelProviderInfo;
    use codex_core::WireApi;

    let gemini_provider = ModelProviderInfo {
        name: "Gemini Test".to_string(),
        base_url: Some("https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-flash:streamGenerateContent".to_string()),
        env_key: Some("GEMINI_API_KEY".to_string()),
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: WireApi::Gemini,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        requires_openai_auth: false,
    };

    config
        .model_providers
        .insert("gemini-test".to_string(), gemini_provider);
    config.model_provider_id = "gemini-test".to_string();
    config.model = "gemini-1.5-flash".to_string();
    config.approval_policy = codex_protocol::protocol::AskForApproval::Never;
    config.sandbox_policy = codex_protocol::protocol::SandboxPolicy::DangerFullAccess;

    let auth_manager = Arc::new(AuthManager::new(
        config.codex_home.clone(),
        true,
        AuthCredentialsStoreMode::Auto,
    ));

    let spawn_ok = Codex::spawn(
        config,
        auth_manager,
        InitialHistory::New,
        SessionSource::Exec,
    )
    .await?;

    let codex = spawn_ok.codex;

    // Turn 1: Ask for edit
    let turn_id = codex.submit(Op::UserTurn {
        items: vec![codex_protocol::user_input::UserInput::Text {
            text: "Edit context.py: In the Logger class, inside the process method, change 'self.log(\"start\")' to 'self.log(\"init\")'. ensure you are modifying the Logger class and not the Calculator class. Use apply_patch for this.".to_string()
        }],
        cwd: dir.path().to_path_buf(),
        approval_policy: codex_protocol::protocol::AskForApproval::Never,
        sandbox_policy: codex_protocol::protocol::SandboxPolicy::DangerFullAccess,
        model: "gemini-1.5-flash".to_string(),
        effort: None,
        summary: codex_protocol::config_types::ReasoningSummary::None,
        final_output_json_schema: None,
    }).await?;

    let mut saw_diff = false;
    loop {
        let event = codex.next_event().await?;
        println!("Event: {:?}", event.msg);
        match event.msg {
            EventMsg::TaskComplete(_) => {
                assert_eq!(event.id, turn_id);
                break;
            }
            EventMsg::TurnAborted(aborted) => {
                panic!("Turn aborted: {:?}", aborted);
            }
            EventMsg::TurnDiff(_) => {
                saw_diff = true;
            }
            _ => {}
        }
    }

    assert!(
        saw_diff,
        "Expected to see a TurnDiff event indicating successful patch"
    );

    let contents = std::fs::read_to_string(&context_py)?;
    assert!(
        contents.contains("class Logger:"),
        "File content seems corrupted"
    );
    // Check that Logger was updated
    assert!(contents.contains("class Logger:\n    def process(self):\n        print(\"processing\")\n        self.log(\"init\")"), "Logger class not updated correctly:\n{}", contents);
    // Check that Calculator was NOT updated
    assert!(contents.contains("class Calculator:\n    def process(self):\n        print(\"processing\")\n        self.log(\"start\")"), "Calculator class incorrectly updated:\n{}", contents);

    Ok(())
}
