use super::cache::ModelsCacheManager;
use crate::api_bridge::auth_provider_from_auth;
use crate::api_bridge::map_api_error;
use crate::auth::AuthManager;
use crate::auth::AuthMode;
use crate::config::Config;
use crate::default_client::build_reqwest_client;
use crate::error::CodexErr;
use crate::error::Result as CoreResult;
use crate::features::Feature;
use crate::model_provider_info::ModelProviderInfo;
use crate::models_manager::collaboration_mode_presets::builtin_collaboration_mode_presets;
use crate::models_manager::model_info;
use crate::models_manager::model_presets::builtin_model_presets;
use codex_api::ModelsClient;
use codex_api::ReqwestTransport;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use http::HeaderMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::sync::TryLockError;
use tokio::time::timeout;
use tracing::error;

const MODEL_CACHE_FILE: &str = "models_cache.json";
const DEFAULT_MODEL_CACHE_TTL: Duration = Duration::from_secs(300);
const MODELS_REFRESH_TIMEOUT: Duration = Duration::from_secs(5);

/// Strategy for refreshing available models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshStrategy {
    /// Always fetch from the network, ignoring cache.
    Online,
    /// Only use cached data, never fetch from the network.
    Offline,
    /// Use cache if available and fresh, otherwise fetch from the network.
    OnlineIfUncached,
}

/// Coordinates remote model discovery plus cached metadata on disk.
#[derive(Debug)]
pub struct ModelsManager {
    local_models: Vec<ModelPreset>,
    remote_models: RwLock<Vec<ModelInfo>>,
    auth_manager: Arc<AuthManager>,
    etag: RwLock<Option<String>>,
    cache_manager: ModelsCacheManager,
    provider: ModelProviderInfo,
}

impl ModelsManager {
    /// Construct a manager scoped to the provided `AuthManager` and model provider.
    ///
    /// Uses `codex_home` to store provider-scoped cached model metadata and initializes with
    /// built-in presets.
    pub fn new(
        codex_home: PathBuf,
        auth_manager: Arc<AuthManager>,
        model_provider_id: &str,
        provider: ModelProviderInfo,
    ) -> Self {
        let cache_path = models_cache_path(&codex_home, model_provider_id);
        let cache_manager = ModelsCacheManager::new(cache_path, DEFAULT_MODEL_CACHE_TTL);
        Self {
            local_models: builtin_model_presets(auth_manager.get_internal_auth_mode()),
            remote_models: RwLock::new(Self::load_remote_models_from_file().unwrap_or_default()),
            auth_manager,
            etag: RwLock::new(None),
            cache_manager,
            provider,
        }
    }

    /// List all available models, refreshing according to the specified strategy.
    ///
    /// Returns model presets sorted by priority and filtered by auth mode and visibility.
    pub async fn list_models(
        &self,
        config: &Config,
        refresh_strategy: RefreshStrategy,
    ) -> Vec<ModelPreset> {
        if let Err(err) = self
            .refresh_available_models(config, refresh_strategy)
            .await
        {
            error!("failed to refresh available models: {err}");
        }
        let remote_models = self.get_remote_models(config).await;
        self.build_available_models(remote_models)
    }

    /// List the models that should be shown in picker UIs.
    ///
    /// Includes the current configured model when it is not otherwise visible in
    /// the picker. Without any configured auth, the picker collapses down to the
    /// configured model when one is present.
    pub async fn list_picker_models(
        &self,
        config: &Config,
        refresh_strategy: RefreshStrategy,
    ) -> Vec<ModelPreset> {
        let available_models = self.list_models(config, refresh_strategy).await;
        self.build_picker_models(config, available_models)
    }

    /// List collaboration mode presets.
    ///
    /// Returns a static set of presets seeded with the configured model.
    pub fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        builtin_collaboration_mode_presets()
    }

    /// Attempt to list models without blocking, using the current cached state.
    ///
    /// Returns an error if the internal lock cannot be acquired.
    pub fn try_list_models(&self, config: &Config) -> Result<Vec<ModelPreset>, TryLockError> {
        let remote_models = self.try_get_remote_models(config)?;
        Ok(self.build_available_models(remote_models))
    }

    /// Attempt to list picker-visible models without blocking, using cached state.
    pub fn try_list_picker_models(
        &self,
        config: &Config,
    ) -> Result<Vec<ModelPreset>, TryLockError> {
        let available_models = self.try_list_models(config)?;
        Ok(self.build_picker_models(config, available_models))
    }

    // todo(aibrahim): should be visible to core only and sent on session_configured event
    /// Get the model identifier to use, refreshing according to the specified strategy.
    ///
    /// If `model` is provided, returns it directly. Otherwise selects the default based on
    /// auth mode and available models.
    pub async fn get_default_model(
        &self,
        model: &Option<String>,
        config: &Config,
        refresh_strategy: RefreshStrategy,
    ) -> String {
        if let Some(model) = model.as_ref() {
            return model.to_string();
        }
        if let Err(err) = self
            .refresh_available_models(config, refresh_strategy)
            .await
        {
            error!("failed to refresh available models: {err}");
        }
        let remote_models = self.get_remote_models(config).await;
        let available = self.build_available_models(remote_models);
        available
            .iter()
            .find(|model| model.is_default)
            .or_else(|| available.first())
            .map(|model| model.model.clone())
            .unwrap_or_default()
    }

    // todo(aibrahim): look if we can tighten it to pub(crate)
    /// Look up model metadata, applying remote overrides and config adjustments.
    pub async fn get_model_info(&self, model: &str, config: &Config) -> ModelInfo {
        let remote = self
            .get_remote_models(config)
            .await
            .into_iter()
            .find(|m| m.slug == model);
        let model = if let Some(remote) = remote {
            remote
        } else {
            model_info::find_model_info_for_slug(model)
        };
        model_info::with_config_overrides(model, config)
    }

    /// Refresh models if the provided ETag differs from the cached ETag.
    ///
    /// Uses `Online` strategy to fetch latest models when ETags differ.
    pub(crate) async fn refresh_if_new_etag(&self, etag: String, config: &Config) {
        let current_etag = self.get_etag().await;
        if current_etag.clone().is_some() && current_etag.as_deref() == Some(etag.as_str()) {
            if let Err(err) = self.cache_manager.renew_cache_ttl().await {
                error!("failed to renew cache TTL: {err}");
            }
            return;
        }
        if let Err(err) = self
            .refresh_available_models(config, RefreshStrategy::Online)
            .await
        {
            error!("failed to refresh available models: {err}");
        }
    }

    /// Refresh available models according to the specified strategy.
    async fn refresh_available_models(
        &self,
        config: &Config,
        refresh_strategy: RefreshStrategy,
    ) -> CoreResult<()> {
        if !config.features.enabled(Feature::RemoteModels)
            || self.auth_manager.get_internal_auth_mode() == Some(AuthMode::ApiKey)
        {
            return Ok(());
        }

        match refresh_strategy {
            RefreshStrategy::Offline => {
                // Only try to load from cache, never fetch
                self.try_load_cache().await;
                Ok(())
            }
            RefreshStrategy::OnlineIfUncached => {
                // Try cache first, fall back to online if unavailable
                if self.try_load_cache().await {
                    return Ok(());
                }
                self.fetch_and_update_models().await
            }
            RefreshStrategy::Online => {
                // Always fetch from network
                self.fetch_and_update_models().await
            }
        }
    }

    async fn fetch_and_update_models(&self) -> CoreResult<()> {
        let _timer =
            codex_otel::start_global_timer("codex.remote_models.fetch_update.duration_ms", &[]);
        let auth = self.auth_manager.auth().await;
        let auth_mode = self.auth_manager.get_internal_auth_mode();
        let api_provider = self.provider.to_api_provider(auth_mode)?;
        let api_auth = auth_provider_from_auth(auth.clone(), &self.provider)?;
        let transport = ReqwestTransport::new(build_reqwest_client());
        let client = ModelsClient::new(transport, api_provider, api_auth);

        let client_version = format_client_version_to_whole();
        let (models, etag) = timeout(
            MODELS_REFRESH_TIMEOUT,
            client.list_models(&client_version, HeaderMap::new()),
        )
        .await
        .map_err(|_| CodexErr::Timeout)?
        .map_err(map_api_error)?;

        self.apply_remote_models(models.clone()).await;
        *self.etag.write().await = etag.clone();
        self.cache_manager.persist_cache(&models, etag).await;
        Ok(())
    }

    async fn get_etag(&self) -> Option<String> {
        self.etag.read().await.clone()
    }

    /// Replace the cached remote models and rebuild the derived presets list.
    async fn apply_remote_models(&self, models: Vec<ModelInfo>) {
        let mut existing_models = Self::load_remote_models_from_file().unwrap_or_default();
        for model in models {
            if let Some(existing_index) = existing_models
                .iter()
                .position(|existing| existing.slug == model.slug)
            {
                existing_models[existing_index] = model;
            } else {
                existing_models.push(model);
            }
        }
        *self.remote_models.write().await = existing_models;
    }

    fn load_remote_models_from_file() -> Result<Vec<ModelInfo>, std::io::Error> {
        let file_contents = include_str!("../../models.json");
        let response: ModelsResponse = serde_json::from_str(file_contents)?;
        Ok(response.models)
    }

    /// Attempt to satisfy the refresh from the cache when it matches the provider and TTL.
    async fn try_load_cache(&self) -> bool {
        let _timer =
            codex_otel::start_global_timer("codex.remote_models.load_cache.duration_ms", &[]);
        let cache = match self.cache_manager.load_fresh().await {
            Some(cache) => cache,
            None => return false,
        };
        let models = cache.models.clone();
        *self.etag.write().await = cache.etag.clone();
        self.apply_remote_models(models.clone()).await;
        true
    }

    /// Merge remote model metadata into picker-ready presets, preserving existing entries.
    fn build_available_models(&self, mut remote_models: Vec<ModelInfo>) -> Vec<ModelPreset> {
        remote_models.sort_by(|a, b| a.priority.cmp(&b.priority));

        let remote_presets: Vec<ModelPreset> = remote_models.into_iter().map(Into::into).collect();
        let existing_presets = self.local_models.clone();
        let mut merged_presets = ModelPreset::merge(remote_presets, existing_presets);
        let chatgpt_mode = matches!(
            self.auth_manager.get_internal_auth_mode(),
            Some(AuthMode::Chatgpt)
        );
        merged_presets = ModelPreset::filter_by_auth(merged_presets, chatgpt_mode);

        for preset in &mut merged_presets {
            preset.is_default = false;
        }
        if let Some(default) = merged_presets
            .iter_mut()
            .find(|preset| preset.show_in_picker)
        {
            default.is_default = true;
        } else if let Some(default) = merged_presets.first_mut() {
            default.is_default = true;
        }

        merged_presets
    }

    fn build_picker_models(
        &self,
        config: &Config,
        available_models: Vec<ModelPreset>,
    ) -> Vec<ModelPreset> {
        let mut picker_models: Vec<ModelPreset> = available_models
            .into_iter()
            .filter(|preset| preset.show_in_picker)
            .collect();
        let has_auth =
            self.auth_manager.get_internal_auth_mode().is_some() || self.provider.has_local_auth();

        let custom_model = self
            .configured_picker_model(config, &picker_models)
            .filter(|preset| {
                !picker_models
                    .iter()
                    .any(|model| model.model == preset.model)
            });

        match (has_auth, custom_model) {
            (true, Some(custom_model)) => {
                picker_models.push(custom_model);
                picker_models
            }
            (true, None) => picker_models,
            (false, Some(mut custom_model)) => {
                custom_model.is_default = true;
                vec![custom_model]
            }
            (false, None) => picker_models,
        }
    }

    fn configured_picker_model(
        &self,
        config: &Config,
        picker_models: &[ModelPreset],
    ) -> Option<ModelPreset> {
        let model = config.model.as_deref()?.trim();
        if model.is_empty() || picker_models.iter().any(|preset| preset.model == model) {
            return None;
        }

        let model_info =
            model_info::with_config_overrides(model_info::find_model_info_for_slug(model), config);
        let default_reasoning_effort = config
            .model_reasoning_effort
            .or(model_info.default_reasoning_level)
            .unwrap_or(ReasoningEffort::Medium);
        let supports_personality = model_info.supports_personality();

        Some(ModelPreset {
            id: model.to_string(),
            model: model.to_string(),
            display_name: model.to_string(),
            description: format!(
                "Configured model from config.toml for provider {}.",
                config.model_provider_id
            ),
            default_reasoning_effort,
            supported_reasoning_efforts: model_info.supported_reasoning_levels,
            supports_personality,
            is_default: false,
            upgrade: None,
            show_in_picker: true,
            supported_in_api: true,
        })
    }

    pub fn is_configured_custom_model(
        model: &str,
        config: &Config,
        auth_mode: Option<AuthMode>,
    ) -> bool {
        let model = model.trim();
        let Some(config_model) = config
            .model
            .as_deref()
            .map(str::trim)
            .filter(|configured_model| !configured_model.is_empty())
        else {
            return false;
        };

        if config_model != model {
            return false;
        }

        let local_presets = builtin_model_presets(auth_mode);
        let remote_presets: Vec<ModelPreset> = Self::load_remote_models_from_file()
            .map(|response_models| response_models.into_iter().map(Into::into).collect())
            .unwrap_or_default();
        let picker_models = ModelPreset::filter_by_auth(
            ModelPreset::merge(remote_presets, local_presets),
            matches!(auth_mode, Some(AuthMode::Chatgpt)),
        )
        .into_iter()
        .filter(|preset| preset.show_in_picker)
        .collect::<Vec<_>>();

        !picker_models
            .iter()
            .any(|preset| preset.model == config_model)
    }

    async fn get_remote_models(&self, config: &Config) -> Vec<ModelInfo> {
        if config.features.enabled(Feature::RemoteModels) {
            self.remote_models.read().await.clone()
        } else {
            Vec::new()
        }
    }

    fn try_get_remote_models(&self, config: &Config) -> Result<Vec<ModelInfo>, TryLockError> {
        if config.features.enabled(Feature::RemoteModels) {
            Ok(self.remote_models.try_read()?.clone())
        } else {
            Ok(Vec::new())
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Construct a manager with a specific provider for testing.
    pub fn with_provider(
        codex_home: PathBuf,
        auth_manager: Arc<AuthManager>,
        model_provider_id: &str,
        provider: ModelProviderInfo,
    ) -> Self {
        Self::new(codex_home, auth_manager, model_provider_id, provider)
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Get model identifier without consulting remote state or cache.
    pub fn get_model_offline(model: Option<&str>) -> String {
        if let Some(model) = model {
            return model.to_string();
        }
        let presets = builtin_model_presets(None);
        presets
            .iter()
            .find(|preset| preset.show_in_picker)
            .or_else(|| presets.first())
            .map(|preset| preset.model.clone())
            .unwrap_or_default()
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Build `ModelInfo` without consulting remote state or cache.
    pub fn construct_model_info_offline(model: &str, config: &Config) -> ModelInfo {
        model_info::with_config_overrides(model_info::find_model_info_for_slug(model), config)
    }
}

fn models_cache_path(codex_home: &std::path::Path, model_provider_id: &str) -> PathBuf {
    codex_home
        .join("remote_models")
        .join(sanitize_model_provider_id(model_provider_id))
        .join(MODEL_CACHE_FILE)
}

fn sanitize_model_provider_id(model_provider_id: &str) -> String {
    model_provider_id
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => ch,
            _ => '_',
        })
        .collect()
}

/// Convert a client version string to a whole version string (e.g. "1.2.3-alpha.4" -> "1.2.3")
fn format_client_version_to_whole() -> String {
    format!(
        "{}.{}.{}",
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR"),
        env!("CARGO_PKG_VERSION_PATCH")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CodexAuth;
    use crate::auth::AuthCredentialsStoreMode;
    use crate::config::ConfigBuilder;
    use crate::features::Feature;
    use crate::model_provider_info::WireApi;
    use chrono::Utc;
    use codex_protocol::openai_models::ModelsResponse;
    use core_test_support::responses::mount_models_once;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tempfile::tempdir;
    use wiremock::MockServer;

    fn remote_model(slug: &str, display: &str, priority: i32) -> ModelInfo {
        remote_model_with_visibility(slug, display, priority, "list")
    }

    fn remote_model_with_visibility(
        slug: &str,
        display: &str,
        priority: i32,
        visibility: &str,
    ) -> ModelInfo {
        serde_json::from_value(json!({
            "slug": slug,
            "display_name": display,
            "description": format!("{display} desc"),
            "default_reasoning_level": "medium",
            "supported_reasoning_levels": [{"effort": "low", "description": "low"}, {"effort": "medium", "description": "medium"}],
            "shell_type": "shell_command",
            "visibility": visibility,
            "minimal_client_version": [0, 1, 0],
            "supported_in_api": true,
            "priority": priority,
            "upgrade": null,
            "base_instructions": "base instructions",
            "supports_reasoning_summaries": false,
            "support_verbosity": false,
            "default_verbosity": null,
            "apply_patch_tool_type": null,
            "truncation_policy": {"mode": "bytes", "limit": 10_000},
            "supports_parallel_tool_calls": false,
            "context_window": 272_000,
            "experimental_supported_tools": [],
        }))
        .expect("valid model")
    }

    fn assert_models_contain(actual: &[ModelInfo], expected: &[ModelInfo]) {
        for model in expected {
            assert!(
                actual.iter().any(|candidate| candidate.slug == model.slug),
                "expected model {} in cached list",
                model.slug
            );
        }
    }

    fn provider_for(base_url: String) -> ModelProviderInfo {
        ModelProviderInfo {
            name: "mock".into(),
            base_url: Some(base_url),
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: Some(0),
            stream_max_retries: Some(0),
            stream_idle_timeout_ms: Some(5_000),
            requires_openai_auth: false,
            supports_websockets: false,
        }
    }

    #[tokio::test]
    async fn refresh_available_models_sorts_by_priority() {
        core_test_support::skip_if_sandbox!();

        let server = MockServer::start().await;
        let remote_models = vec![
            remote_model("priority-low", "Low", 1),
            remote_model("priority-high", "High", 0),
        ];
        let models_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: remote_models.clone(),
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        config.features.enable(Feature::RemoteModels);
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        let provider = provider_for(server.uri());
        let manager = ModelsManager::with_provider(
            codex_home.path().to_path_buf(),
            auth_manager,
            "mock-provider",
            provider,
        );

        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("refresh succeeds");
        let cached_remote = manager.get_remote_models(&config).await;
        assert_models_contain(&cached_remote, &remote_models);

        let available = manager
            .list_models(&config, RefreshStrategy::OnlineIfUncached)
            .await;
        let high_idx = available
            .iter()
            .position(|model| model.model == "priority-high")
            .expect("priority-high should be listed");
        let low_idx = available
            .iter()
            .position(|model| model.model == "priority-low")
            .expect("priority-low should be listed");
        assert!(
            high_idx < low_idx,
            "higher priority should be listed before lower priority"
        );
        assert_eq!(
            models_mock.requests().len(),
            1,
            "expected a single /models request"
        );
    }

    #[tokio::test]
    async fn new_uses_supplied_provider_for_remote_model_refresh() {
        core_test_support::skip_if_sandbox!();

        let server = MockServer::start().await;
        let remote_models = vec![remote_model("custom-provider-model", "Custom Provider", 1)];
        let models_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: remote_models.clone(),
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        config.features.enable(Feature::RemoteModels);
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        let provider = provider_for(server.uri());
        let manager = ModelsManager::new(
            codex_home.path().to_path_buf(),
            auth_manager,
            "custom-provider",
            provider,
        );

        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("refresh succeeds");
        assert_models_contain(&manager.get_remote_models(&config).await, &remote_models);
        assert_eq!(
            models_mock.requests().len(),
            1,
            "expected a single /models request against the supplied provider"
        );
    }

    #[tokio::test]
    async fn refresh_available_models_uses_cache_when_fresh() {
        core_test_support::skip_if_sandbox!();

        let server = MockServer::start().await;
        let remote_models = vec![remote_model("cached", "Cached", 5)];
        let models_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: remote_models.clone(),
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        config.features.enable(Feature::RemoteModels);
        let auth_manager = Arc::new(AuthManager::new(
            codex_home.path().to_path_buf(),
            false,
            AuthCredentialsStoreMode::File,
        ));
        let provider = provider_for(server.uri());
        let manager = ModelsManager::with_provider(
            codex_home.path().to_path_buf(),
            auth_manager,
            "mock-provider",
            provider,
        );

        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("first refresh succeeds");
        assert_models_contain(&manager.get_remote_models(&config).await, &remote_models);

        // Second call should read from cache and avoid the network.
        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("cached refresh succeeds");
        assert_models_contain(&manager.get_remote_models(&config).await, &remote_models);
        assert_eq!(
            models_mock.requests().len(),
            1,
            "cache hit should avoid a second /models request"
        );
    }

    #[tokio::test]
    async fn refresh_available_models_scopes_cache_by_provider() {
        core_test_support::skip_if_sandbox!();

        let server_a = MockServer::start().await;
        let models_a = vec![remote_model("provider-a", "Provider A", 1)];
        let mock_a = mount_models_once(
            &server_a,
            ModelsResponse {
                models: models_a.clone(),
            },
        )
        .await;

        let server_b = MockServer::start().await;
        let models_b = vec![remote_model("provider-b", "Provider B", 1)];
        let mock_b = mount_models_once(
            &server_b,
            ModelsResponse {
                models: models_b.clone(),
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        config.features.enable(Feature::RemoteModels);
        let auth_manager = Arc::new(AuthManager::new(
            codex_home.path().to_path_buf(),
            false,
            AuthCredentialsStoreMode::File,
        ));

        let manager_a = ModelsManager::with_provider(
            codex_home.path().to_path_buf(),
            Arc::clone(&auth_manager),
            "mock-provider-a",
            provider_for(server_a.uri()),
        );
        manager_a
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("provider A refresh succeeds");
        assert_models_contain(&manager_a.get_remote_models(&config).await, &models_a);

        let manager_b = ModelsManager::with_provider(
            codex_home.path().to_path_buf(),
            auth_manager,
            "mock-provider-b",
            provider_for(server_b.uri()),
        );
        manager_b
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("provider B refresh succeeds");

        let remote_models = manager_b.get_remote_models(&config).await;
        assert_models_contain(&remote_models, &models_b);
        assert!(
            !remote_models.iter().any(|model| model.slug == "provider-a"),
            "provider B should not reuse provider A cache"
        );
        assert_eq!(
            mock_a.requests().len(),
            1,
            "provider A should fetch /models once"
        );
        assert_eq!(
            mock_b.requests().len(),
            1,
            "provider B should fetch /models once instead of reusing provider A cache"
        );
    }

    #[tokio::test]
    async fn refresh_available_models_refetches_when_cache_stale() {
        core_test_support::skip_if_sandbox!();

        let server = MockServer::start().await;
        let initial_models = vec![remote_model("stale", "Stale", 1)];
        let initial_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: initial_models.clone(),
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        config.features.enable(Feature::RemoteModels);
        let auth_manager = Arc::new(AuthManager::new(
            codex_home.path().to_path_buf(),
            false,
            AuthCredentialsStoreMode::File,
        ));
        let provider = provider_for(server.uri());
        let manager = ModelsManager::with_provider(
            codex_home.path().to_path_buf(),
            auth_manager,
            "mock-provider",
            provider,
        );

        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("initial refresh succeeds");

        // Rewrite cache with an old timestamp so it is treated as stale.
        manager
            .cache_manager
            .manipulate_cache_for_test(|fetched_at| {
                *fetched_at = Utc::now() - chrono::Duration::hours(1);
            })
            .await
            .expect("cache manipulation succeeds");

        let updated_models = vec![remote_model("fresh", "Fresh", 9)];
        server.reset().await;
        let refreshed_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: updated_models.clone(),
            },
        )
        .await;

        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("second refresh succeeds");
        assert_models_contain(&manager.get_remote_models(&config).await, &updated_models);
        assert_eq!(
            initial_mock.requests().len(),
            1,
            "initial refresh should only hit /models once"
        );
        assert_eq!(
            refreshed_mock.requests().len(),
            1,
            "stale cache refresh should fetch /models once"
        );
    }

    #[tokio::test]
    async fn refresh_available_models_drops_removed_remote_models() {
        core_test_support::skip_if_sandbox!();

        let server = MockServer::start().await;
        let initial_models = vec![remote_model("remote-old", "Remote Old", 1)];
        let initial_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: initial_models,
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        config.features.enable(Feature::RemoteModels);
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        let provider = provider_for(server.uri());
        let mut manager = ModelsManager::with_provider(
            codex_home.path().to_path_buf(),
            auth_manager,
            "mock-provider",
            provider,
        );
        manager.cache_manager.set_ttl(Duration::ZERO);

        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("initial refresh succeeds");

        server.reset().await;
        let refreshed_models = vec![remote_model("remote-new", "Remote New", 1)];
        let refreshed_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: refreshed_models,
            },
        )
        .await;

        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("second refresh succeeds");

        let available = manager
            .try_list_models(&config)
            .expect("models should be available");
        assert!(
            available.iter().any(|preset| preset.model == "remote-new"),
            "new remote model should be listed"
        );
        assert!(
            !available.iter().any(|preset| preset.model == "remote-old"),
            "removed remote model should not be listed"
        );
        assert_eq!(
            initial_mock.requests().len(),
            1,
            "initial refresh should only hit /models once"
        );
        assert_eq!(
            refreshed_mock.requests().len(),
            1,
            "second refresh should only hit /models once"
        );
    }

    #[test]
    fn build_available_models_picks_default_after_hiding_hidden_models() {
        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let provider = provider_for("http://example.test".to_string());
        let mut manager = ModelsManager::with_provider(
            codex_home.path().to_path_buf(),
            auth_manager,
            "mock-provider",
            provider,
        );
        manager.local_models = Vec::new();

        let hidden_model = remote_model_with_visibility("hidden", "Hidden", 0, "hide");
        let visible_model = remote_model_with_visibility("visible", "Visible", 1, "list");

        let expected_hidden = ModelPreset::from(hidden_model.clone());
        let mut expected_visible = ModelPreset::from(visible_model.clone());
        expected_visible.is_default = true;

        let available = manager.build_available_models(vec![hidden_model, visible_model]);

        assert_eq!(available, vec![expected_hidden, expected_visible]);
    }

    #[test]
    fn bundled_models_json_roundtrips() {
        let file_contents = include_str!("../../models.json");
        let response: ModelsResponse =
            serde_json::from_str(file_contents).expect("bundled models.json should deserialize");

        let serialized =
            serde_json::to_string(&response).expect("bundled models.json should serialize");
        let roundtripped: ModelsResponse =
            serde_json::from_str(&serialized).expect("serialized models.json should deserialize");

        assert_eq!(
            response, roundtripped,
            "bundled models.json should round trip through serde"
        );
        assert!(
            !response.models.is_empty(),
            "bundled models.json should contain at least one model"
        );
    }

    #[test]
    fn models_cache_path_sanitizes_provider_id() {
        let path = models_cache_path(std::path::Path::new("/tmp/codey"), "mock/provider:beta");
        assert_eq!(
            path,
            PathBuf::from("/tmp/codey/remote_models/mock_provider_beta/models_cache.json")
        );
    }

    #[tokio::test]
    async fn list_picker_models_without_auth_returns_only_configured_custom_model() {
        let codex_home = tempdir().expect("temp dir");
        let auth_manager = Arc::new(AuthManager::new(
            codex_home.path().to_path_buf(),
            false,
            AuthCredentialsStoreMode::File,
        ));
        let manager = ModelsManager::new(
            codex_home.path().to_path_buf(),
            auth_manager,
            "openai",
            ModelProviderInfo::create_openai_provider(),
        );
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        config.model = Some("mock-model".to_string());
        config.model_provider_id = "mock-provider".to_string();

        let picker_models = manager
            .list_picker_models(&config, RefreshStrategy::Offline)
            .await;

        assert_eq!(picker_models.len(), 1);
        assert_eq!(picker_models[0].model, "mock-model");
        assert_eq!(picker_models[0].display_name, "mock-model");
        assert_eq!(
            picker_models[0].description,
            "Configured model from config.toml for provider mock-provider."
        );
        assert!(picker_models[0].is_default);
        assert!(picker_models[0].show_in_picker);
    }

    #[tokio::test]
    async fn list_picker_models_with_auth_appends_configured_custom_model() {
        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        let manager = ModelsManager::new(
            codex_home.path().to_path_buf(),
            auth_manager,
            "openai",
            ModelProviderInfo::create_openai_provider(),
        );
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        config.model = Some("mock-model".to_string());
        config.model_provider_id = "mock-provider".to_string();

        let picker_models = manager
            .list_picker_models(&config, RefreshStrategy::Offline)
            .await;

        assert_eq!(
            picker_models.first().map(|preset| preset.model.as_str()),
            Some("gpt-5.2-codex")
        );
        assert_eq!(
            picker_models.last().map(|preset| preset.model.as_str()),
            Some("mock-model")
        );
        assert_eq!(
            picker_models
                .iter()
                .filter(|preset| preset.is_default)
                .count(),
            1
        );
        assert!(
            picker_models
                .iter()
                .any(|preset| preset.model == "gpt-5.2-codex" && preset.is_default)
        );
    }

    #[tokio::test]
    async fn list_picker_models_with_provider_bearer_token_appends_configured_custom_model() {
        let codex_home = tempdir().expect("temp dir");
        let auth_manager = Arc::new(AuthManager::new(
            codex_home.path().to_path_buf(),
            false,
            AuthCredentialsStoreMode::File,
        ));
        let mut provider = provider_for("http://example.test".to_string());
        provider.experimental_bearer_token = Some("sk-test".to_string());
        let manager = ModelsManager::with_provider(
            codex_home.path().to_path_buf(),
            auth_manager,
            "mock-provider",
            provider,
        );
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        config.model = Some("mock-model".to_string());
        config.model_provider_id = "mock-provider".to_string();

        let picker_models = manager
            .list_picker_models(&config, RefreshStrategy::Offline)
            .await;

        assert_eq!(
            picker_models.first().map(|preset| preset.model.as_str()),
            Some("gpt-5.2-codex")
        );
        assert_eq!(
            picker_models.last().map(|preset| preset.model.as_str()),
            Some("mock-model")
        );
    }

    #[tokio::test]
    async fn list_picker_models_with_provider_env_key_appends_configured_custom_model() {
        let codex_home = tempdir().expect("temp dir");
        let auth_manager = Arc::new(AuthManager::new(
            codex_home.path().to_path_buf(),
            false,
            AuthCredentialsStoreMode::File,
        ));
        let mut provider = provider_for("http://example.test".to_string());
        provider.env_key = Some("PATH".to_string());
        let manager = ModelsManager::with_provider(
            codex_home.path().to_path_buf(),
            auth_manager,
            "mock-provider",
            provider,
        );
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        config.model = Some("mock-model".to_string());
        config.model_provider_id = "mock-provider".to_string();

        let picker_models = manager
            .list_picker_models(&config, RefreshStrategy::Offline)
            .await;

        assert_eq!(
            picker_models.first().map(|preset| preset.model.as_str()),
            Some("gpt-5.2-codex")
        );
        assert_eq!(
            picker_models.last().map(|preset| preset.model.as_str()),
            Some("mock-model")
        );
    }

    #[tokio::test]
    async fn configured_custom_model_detection_matches_picker_behavior() {
        let codex_home = tempdir().expect("temp dir");
        let auth_manager = Arc::new(AuthManager::new(
            codex_home.path().to_path_buf(),
            false,
            AuthCredentialsStoreMode::File,
        ));
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");

        config.model = Some("mock-model".to_string());
        assert!(ModelsManager::is_configured_custom_model(
            "mock-model",
            &config,
            auth_manager.get_internal_auth_mode(),
        ));

        config.model = Some("gpt-5.2-codex".to_string());
        assert!(!ModelsManager::is_configured_custom_model(
            "gpt-5.2-codex",
            &config,
            auth_manager.get_internal_auth_mode(),
        ));
    }
}
