use anyhow::Result;
use app_test_support::McpProcess;
use app_test_support::to_response;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SetDefaultModelParams;
use codex_app_server_protocol::SetDefaultModelResponse;
use codex_core::config::ConfigToml;
use pretty_assertions::assert_eq;
use std::path::Path;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_default_model_persists_overrides() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path())?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let params = SetDefaultModelParams {
        model: Some("gpt-4.1".to_string()),
        model_provider: None,
        reasoning_effort: None,
    };

    let request_id = mcp.send_set_default_model_request(params).await?;

    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;

    let _: SetDefaultModelResponse = to_response(resp)?;

    let config_path = codex_home.path().join("config.toml");
    let config_contents = tokio::fs::read_to_string(&config_path).await?;
    let config_toml: ConfigToml = toml::from_str(&config_contents)?;

    assert_eq!(
        ConfigToml {
            model: Some("gpt-4.1".to_string()),
            model_provider: Some("openai".to_string()),
            model_reasoning_effort: None,
            ..Default::default()
        },
        config_toml,
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_default_model_persists_explicit_provider() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml_with_custom_provider(codex_home.path())?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let params = SetDefaultModelParams {
        model: Some("deepseek-v3".to_string()),
        model_provider: Some("iie".to_string()),
        reasoning_effort: None,
    };

    let request_id = mcp.send_set_default_model_request(params).await?;

    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;

    let _: SetDefaultModelResponse = to_response(resp)?;

    let config_path = codex_home.path().join("config.toml");
    let config_contents = tokio::fs::read_to_string(&config_path).await?;
    let config_toml: ConfigToml = toml::from_str(&config_contents)?;
    let expected: ConfigToml = toml::from_str(
        r#"
model = "deepseek-v3"
model_provider = "iie"

[model_providers.iie]
name = "iie"
base_url = "https://example.com/iie"
wire_api = "responses"
experimental_bearer_token = "sk-test"
"#,
    )?;

    assert_eq!(expected, config_toml);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_default_model_infers_provider_from_saved_profiles() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_profile_mapped_config_toml(codex_home.path())?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let params = SetDefaultModelParams {
        model: Some("deepseek-v3".to_string()),
        model_provider: None,
        reasoning_effort: None,
    };

    let request_id = mcp.send_set_default_model_request(params).await?;

    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;

    let _: SetDefaultModelResponse = to_response(resp)?;

    let config_path = codex_home.path().join("config.toml");
    let config_contents = tokio::fs::read_to_string(&config_path).await?;
    let config_toml: ConfigToml = toml::from_str(&config_contents)?;

    assert_eq!(config_toml.model.as_deref(), Some("deepseek-v3"));
    assert_eq!(config_toml.model_provider.as_deref(), Some("provider_b"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_default_model_rejects_unknown_explicit_provider() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path())?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let params = SetDefaultModelParams {
        model: Some("gpt-4.1".to_string()),
        model_provider: Some("missing-provider".to_string()),
        reasoning_effort: None,
    };

    let request_id = mcp.send_set_default_model_request(params).await?;

    let error: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(
        error.error.message,
        "failed to persist model selection: model provider `missing-provider` was not found"
    );

    let config_path = codex_home.path().join("config.toml");
    let config_contents = tokio::fs::read_to_string(&config_path).await?;
    let config_toml: ConfigToml = toml::from_str(&config_contents)?;

    assert_eq!(
        ConfigToml {
            model: Some("gpt-5.1-codex-max".to_string()),
            model_provider: None,
            model_reasoning_effort: Some(codex_protocol::openai_models::ReasoningEffort::Medium,),
            ..Default::default()
        },
        config_toml,
    );
    Ok(())
}

// Helper to create a config.toml; mirrors create_conversation.rs
fn create_config_toml(codex_home: &Path) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        config_toml,
        r#"
model = "gpt-5.1-codex-max"
model_reasoning_effort = "medium"
"#,
    )
}

fn create_profile_mapped_config_toml(codex_home: &Path) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        config_toml,
        r#"
model = "glm-5"
model_provider = "provider_a"

[model_providers.provider_a]
name = "provider_a"
base_url = "https://example.com/a"
wire_api = "chat"
experimental_bearer_token = "sk-a"

[model_providers.provider_b]
name = "provider_b"
base_url = "https://example.com/b"
wire_api = "chat"
experimental_bearer_token = "sk-b"

[profiles.deepseek]
model = "deepseek-v3"
model_provider = "provider_b"
"#,
    )
}

fn create_config_toml_with_custom_provider(codex_home: &Path) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        config_toml,
        r#"
model = "gpt-5.1-codex-max"
model_reasoning_effort = "medium"

[model_providers.iie]
name = "iie"
base_url = "https://example.com/iie"
wire_api = "responses"
experimental_bearer_token = "sk-test"
"#,
    )
}
