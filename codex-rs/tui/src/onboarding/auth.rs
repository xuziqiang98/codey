#![allow(clippy::unwrap_used)]

use codex_api::ApiError;
use codex_api::AuthProvider;
use codex_api::ModelsClient;
use codex_api::Provider;
use codex_api::ReqwestTransport;
use codex_api::TransportError;
use codex_api::WireApi as ApiWireApi;
use codex_api::provider::RetryConfig;
use codex_core::AuthManager;
use codex_core::auth::AuthCredentialsStoreMode;
use codex_core::auth::CLIENT_ID;
use codex_core::config::edit::ConfigEdit;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_core::default_client::build_reqwest_client;
use codex_login::DeviceCode;
use codex_login::ServerOptions;
use codex_login::ShutdownHandle;
use codex_login::run_login_server;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::prelude::Widget;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Block;
use ratatui::widgets::BorderType;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use ratatui::widgets::WidgetRef;
use ratatui::widgets::Wrap;
use reqwest::StatusCode;
use reqwest::header::HeaderMap;

use codex_app_server_protocol::AuthMode;
use codex_protocol::config_types::ForcedLoginMethod;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use textwrap::wrap;
use toml_edit::value;

use crate::LoginStatus;
use crate::onboarding::onboarding_screen::KeyboardHandler;
use crate::onboarding::onboarding_screen::StepStateProvider;
use crate::shimmer::shimmer_spans;
use crate::tui::FrameRequester;
use tokio::sync::Notify;

use super::onboarding_screen::StepState;

#[allow(dead_code)]
mod headless_chatgpt_login;

#[derive(Clone)]
pub(crate) enum SignInState {
    PickMode,
    ChatGptContinueInBrowser(ContinueInBrowserState),
    #[allow(dead_code)]
    ChatGptDeviceCode(ContinueWithDeviceCodeState),
    ChatGptSuccessMessage,
    ChatGptSuccess,
    ApiKeyEntry(ApiKeyInputState),
    ApiKeyConfigured,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SignInOption {
    ChatGpt,
    ApiKey,
}

const API_KEY_DISABLED_MESSAGE: &str = "API key login is disabled.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ApiProviderWizardStep {
    #[default]
    ProviderId,
    WireApi,
    BaseUrl,
    ApiKey,
    Model,
}

impl ApiProviderWizardStep {
    const fn index(self) -> usize {
        match self {
            Self::ProviderId => 1,
            Self::WireApi => 2,
            Self::BaseUrl => 3,
            Self::ApiKey => 4,
            Self::Model => 5,
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::ProviderId => "Provider ID",
            Self::WireApi => "Wire API",
            Self::BaseUrl => "Base URL",
            Self::ApiKey => "API Key",
            Self::Model => "Model",
        }
    }

    const fn placeholder(self) -> &'static str {
        match self {
            Self::ProviderId => "my-provider",
            Self::WireApi => "",
            Self::BaseUrl => "https://example.com/v1",
            Self::ApiKey => "Paste or type your API key",
            Self::Model => "gpt-4.1",
        }
    }

    const fn previous(self) -> Option<Self> {
        match self {
            Self::ProviderId => None,
            Self::WireApi => Some(Self::ProviderId),
            Self::BaseUrl => Some(Self::WireApi),
            Self::ApiKey => Some(Self::BaseUrl),
            Self::Model => Some(Self::ApiKey),
        }
    }

    const fn next(self) -> Option<Self> {
        match self {
            Self::ProviderId => Some(Self::WireApi),
            Self::WireApi => Some(Self::BaseUrl),
            Self::BaseUrl => Some(Self::ApiKey),
            Self::ApiKey => Some(Self::Model),
            Self::Model => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ApiProviderWireApi {
    Chat,
    #[default]
    Responses,
}

impl ApiProviderWireApi {
    fn toggle(&mut self) {
        *self = match self {
            Self::Chat => Self::Responses,
            Self::Responses => Self::Chat,
        };
    }

    fn as_config_value(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Responses => "responses",
        }
    }

    fn as_api_wire(self) -> ApiWireApi {
        match self {
            Self::Chat => ApiWireApi::Chat,
            Self::Responses => ApiWireApi::Responses,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Responses => "responses",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::Chat => "Compatible with chat completions style APIs",
            Self::Responses => "Compatible with Responses API style backends",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct ApiKeyInputState {
    provider_id: String,
    wire_api: ApiProviderWireApi,
    base_url: String,
    api_key: String,
    model: String,
    step: ApiProviderWizardStep,
    validating: bool,
    error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CustomProviderConfig {
    provider_id: String,
    wire_api: ApiProviderWireApi,
    base_url: String,
    api_key: String,
    model: String,
}

#[derive(Clone)]
struct StaticBearerAuth {
    api_key: String,
}

impl AuthProvider for StaticBearerAuth {
    fn bearer_token(&self) -> Option<String> {
        Some(self.api_key.clone())
    }
}

#[derive(Clone)]
/// Used to manage the lifecycle of SpawnedLogin and ensure it gets cleaned up.
pub(crate) struct ContinueInBrowserState {
    auth_url: String,
    shutdown_flag: Option<ShutdownHandle>,
}

#[derive(Clone)]
pub(crate) struct ContinueWithDeviceCodeState {
    device_code: Option<DeviceCode>,
    cancel: Option<Arc<Notify>>,
}

impl Drop for ContinueInBrowserState {
    fn drop(&mut self) {
        if let Some(handle) = &self.shutdown_flag {
            handle.shutdown();
        }
    }
}

impl KeyboardHandler for AuthModeWidget {
    fn handle_key_event(&mut self, key_event: KeyEvent) {
        if self.handle_api_key_entry_key_event(&key_event) {
            return;
        }

        match key_event.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_highlight(-1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_highlight(1);
            }
            KeyCode::Char('1') => {
                self.select_option_by_index(0);
            }
            KeyCode::Char('2') => {
                self.select_option_by_index(1);
            }
            KeyCode::Enter => {
                let sign_in_state = { (*self.sign_in_state.read().unwrap()).clone() };
                match sign_in_state {
                    SignInState::PickMode => {
                        self.handle_sign_in_option(self.highlighted_mode);
                    }
                    SignInState::ChatGptSuccessMessage => {
                        *self.sign_in_state.write().unwrap() = SignInState::ChatGptSuccess;
                    }
                    _ => {}
                }
            }
            KeyCode::Esc => {
                tracing::info!("Esc pressed");
                let mut sign_in_state = self.sign_in_state.write().unwrap();
                match &*sign_in_state {
                    SignInState::ChatGptContinueInBrowser(_) => {
                        *sign_in_state = SignInState::PickMode;
                        drop(sign_in_state);
                        self.request_frame.schedule_frame();
                    }
                    SignInState::ChatGptDeviceCode(state) => {
                        if let Some(cancel) = &state.cancel {
                            cancel.notify_one();
                        }
                        *sign_in_state = SignInState::PickMode;
                        drop(sign_in_state);
                        self.request_frame.schedule_frame();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn handle_paste(&mut self, pasted: String) {
        let _ = self.handle_api_key_entry_paste(pasted);
    }
}

#[derive(Clone)]
pub(crate) struct AuthModeWidget {
    pub request_frame: FrameRequester,
    pub highlighted_mode: SignInOption,
    pub error: Option<String>,
    pub sign_in_state: Arc<RwLock<SignInState>>,
    pub codex_home: PathBuf,
    pub cli_auth_credentials_store_mode: AuthCredentialsStoreMode,
    pub login_status: LoginStatus,
    pub auth_manager: Arc<AuthManager>,
    pub forced_chatgpt_workspace_id: Option<String>,
    pub forced_login_method: Option<ForcedLoginMethod>,
    pub animations_enabled: bool,
}

impl AuthModeWidget {
    fn wrapped_line_count(text: &str, width: u16) -> u16 {
        let usable_width = usize::from(width.max(1));
        wrap(text, usable_width)
            .len()
            .try_into()
            .unwrap_or(u16::MAX)
    }

    fn api_key_entry_input_height(&self, width: u16, state: &ApiKeyInputState) -> u16 {
        let inner_width = width.saturating_sub(2).max(1);
        match state.step {
            ApiProviderWizardStep::WireApi => {
                let options = [ApiProviderWireApi::Chat, ApiProviderWireApi::Responses];
                let content_height =
                    options
                        .into_iter()
                        .enumerate()
                        .fold(0u16, |total, (idx, option)| {
                            let label = format!("  {}. {}", idx + 1, option.label());
                            let description = format!("     {}", option.description());
                            total
                                .saturating_add(Self::wrapped_line_count(&label, inner_width))
                                .saturating_add(Self::wrapped_line_count(&description, inner_width))
                        });
                content_height.saturating_add(2)
            }
            _ => {
                let value = self.current_step_value_for_display(state);
                let content = if value.is_empty() {
                    state.step.placeholder().to_string()
                } else {
                    value
                };
                Self::wrapped_line_count(&content, inner_width).saturating_add(2)
            }
        }
    }

    fn is_api_login_allowed(&self) -> bool {
        !matches!(self.forced_login_method, Some(ForcedLoginMethod::Chatgpt))
    }

    fn is_chatgpt_login_allowed(&self) -> bool {
        !matches!(self.forced_login_method, Some(ForcedLoginMethod::Api))
    }

    fn displayed_sign_in_options(&self) -> Vec<SignInOption> {
        let mut options = Vec::new();
        if self.is_chatgpt_login_allowed() {
            options.push(SignInOption::ChatGpt);
        }
        if self.is_api_login_allowed() {
            options.push(SignInOption::ApiKey);
        }
        options
    }

    fn selectable_sign_in_options(&self) -> Vec<SignInOption> {
        let mut options = Vec::new();
        if self.is_chatgpt_login_allowed() {
            options.push(SignInOption::ChatGpt);
        }
        if self.is_api_login_allowed() {
            options.push(SignInOption::ApiKey);
        }
        options
    }

    fn move_highlight(&mut self, delta: isize) {
        let options = self.selectable_sign_in_options();
        if options.is_empty() {
            return;
        }

        let current_index = options
            .iter()
            .position(|option| *option == self.highlighted_mode)
            .unwrap_or(0);
        let next_index =
            (current_index as isize + delta).rem_euclid(options.len() as isize) as usize;
        self.highlighted_mode = options[next_index];
    }

    fn select_option_by_index(&mut self, index: usize) {
        let options = self.displayed_sign_in_options();
        if let Some(option) = options.get(index).copied() {
            self.handle_sign_in_option(option);
        }
    }

    fn handle_sign_in_option(&mut self, option: SignInOption) {
        match option {
            SignInOption::ChatGpt => {
                if self.is_chatgpt_login_allowed() {
                    self.start_chatgpt_login();
                }
            }
            SignInOption::ApiKey => {
                if self.is_api_login_allowed() {
                    self.start_api_key_entry();
                } else {
                    self.disallow_api_login();
                }
            }
        }
    }

    fn disallow_api_login(&mut self) {
        self.highlighted_mode = SignInOption::ChatGpt;
        self.error = Some(API_KEY_DISABLED_MESSAGE.to_string());
        *self.sign_in_state.write().unwrap() = SignInState::PickMode;
        self.request_frame.schedule_frame();
    }

    fn render_pick_mode(&self, area: Rect, buf: &mut Buffer) {
        let mut lines: Vec<Line> = vec![
            Line::from(vec![
                "  ".into(),
                "Sign in with ChatGPT to use Codex as part of your paid plan".into(),
            ]),
            Line::from(vec![
                "  ".into(),
                "or connect an API key for usage-based billing".into(),
            ]),
            "".into(),
        ];

        let create_mode_item = |idx: usize,
                                selected_mode: SignInOption,
                                text: &str,
                                description: &str|
         -> Vec<Line<'static>> {
            let is_selected = self.highlighted_mode == selected_mode;
            let caret = if is_selected { ">" } else { " " };

            let line1 = if is_selected {
                Line::from(vec![
                    format!("{caret} {index}. ", index = idx + 1).cyan().dim(),
                    text.to_string().cyan(),
                ])
            } else {
                format!("  {index}. {text}", index = idx + 1).into()
            };

            let line2 = if is_selected {
                Line::from(format!("     {description}"))
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::DIM)
            } else {
                Line::from(format!("     {description}"))
                    .style(Style::default().add_modifier(Modifier::DIM))
            };

            vec![line1, line2]
        };

        let chatgpt_description = if !self.is_chatgpt_login_allowed() {
            "ChatGPT login is disabled"
        } else {
            "Usage included with Plus, Pro, Team, and Enterprise plans"
        };
        for (idx, option) in self.displayed_sign_in_options().into_iter().enumerate() {
            match option {
                SignInOption::ChatGpt => {
                    lines.extend(create_mode_item(
                        idx,
                        option,
                        "Sign in with ChatGPT",
                        chatgpt_description,
                    ));
                }
                SignInOption::ApiKey => {
                    lines.extend(create_mode_item(
                        idx,
                        option,
                        "Provide your own API key",
                        "Pay for what you use",
                    ));
                }
            }
            lines.push("".into());
        }

        if !self.is_api_login_allowed() {
            lines.push(
                "  API key login is disabled by this workspace. Sign in with ChatGPT to continue."
                    .dim()
                    .into(),
            );
            lines.push("".into());
        }
        lines.push(
            // AE: Following styles.md, this should probably be Cyan because it's a user input tip.
            //     But leaving this for a future cleanup.
            "  Press Enter to continue".dim().into(),
        );
        if let Some(err) = &self.error {
            lines.push("".into());
            lines.push(err.as_str().red().into());
        }

        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }

    fn render_continue_in_browser(&self, area: Rect, buf: &mut Buffer) {
        let mut spans = vec!["  ".into()];
        if self.animations_enabled {
            // Schedule a follow-up frame to keep the shimmer animation going.
            self.request_frame
                .schedule_frame_in(std::time::Duration::from_millis(100));
            spans.extend(shimmer_spans("Finish signing in via your browser"));
        } else {
            spans.push("Finish signing in via your browser".into());
        }
        let mut lines = vec![spans.into(), "".into()];

        let sign_in_state = self.sign_in_state.read().unwrap();
        if let SignInState::ChatGptContinueInBrowser(state) = &*sign_in_state
            && !state.auth_url.is_empty()
        {
            lines.push("  If the link doesn't open automatically, open the following link to authenticate:".into());
            lines.push("".into());
            lines.push(Line::from(vec![
                "  ".into(),
                state.auth_url.as_str().cyan().underlined(),
            ]));
            lines.push("".into());
        }

        lines.push("  Press Esc to cancel".dim().into());
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }

    fn render_chatgpt_success_message(&self, area: Rect, buf: &mut Buffer) {
        let lines = vec![
            "✓ Signed in with your ChatGPT account".fg(Color::Green).into(),
            "".into(),
            "  Before you start:".into(),
            "".into(),
            "  Decide how much autonomy you want to grant Codex".into(),
            Line::from(vec![
                "  For more details see the ".into(),
                "\u{1b}]8;;https://github.com/openai/codex\u{7}Codex docs\u{1b}]8;;\u{7}".underlined(),
            ])
            .dim(),
            "".into(),
            "  Codex can make mistakes".into(),
            "  Review the code it writes and commands it runs".dim().into(),
            "".into(),
            "  Powered by your ChatGPT account".into(),
            Line::from(vec![
                "  Uses your plan's rate limits and ".into(),
                "\u{1b}]8;;https://chatgpt.com/#settings\u{7}training data preferences\u{1b}]8;;\u{7}".underlined(),
            ])
            .dim(),
            "".into(),
            "  Press Enter to continue".fg(Color::Cyan).into(),
        ];

        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }

    fn render_chatgpt_success(&self, area: Rect, buf: &mut Buffer) {
        let lines = vec![
            "✓ Signed in with your ChatGPT account"
                .fg(Color::Green)
                .into(),
        ];

        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }

    fn render_api_key_configured(&self, area: Rect, buf: &mut Buffer) {
        let lines = vec![
            "✓ Custom provider configured".fg(Color::Green).into(),
            "".into(),
            "  Codey will use the provider saved in config.toml.".into(),
        ];

        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }

    fn render_api_key_entry(&self, area: Rect, buf: &mut Buffer, state: &ApiKeyInputState) {
        let input_height = self.api_key_entry_input_height(area.width, state);
        let intro_min_height = if state.validating { 12 } else { 10 };
        let [intro_area, _spacer_area, input_area, footer_area] = Layout::vertical([
            Constraint::Min(intro_min_height),
            Constraint::Length(1),
            Constraint::Length(input_height),
            Constraint::Min(4),
        ])
        .areas(area);

        let mut intro_lines: Vec<Line> = vec![
            vec!["> ".into(), "Configure a custom API provider".bold()].into(),
            "".into(),
            format!("  Step {}/5: {}", state.step.index(), state.step.title()).into(),
            "  This will be written to ~/.codey/config.toml and used as the default provider."
                .into(),
            "".into(),
            format!(
                "  Provider ID: {}",
                display_optional_value(&state.provider_id, "<not set>")
            )
            .into(),
            format!("  Wire API: {}", state.wire_api.label()).into(),
            format!(
                "  Base URL: {}",
                display_optional_value(&state.base_url, "<not set>")
            )
            .into(),
            format!("  API key: {}", mask_secret(&state.api_key)).into(),
            format!(
                "  Model: {}",
                display_optional_value(&state.model, "<not set>")
            )
            .into(),
        ];
        if state.validating {
            intro_lines.push("".into());
            intro_lines.push(
                "  Validating provider settings and saving config.toml..."
                    .cyan()
                    .into(),
            );
        }
        Paragraph::new(intro_lines)
            .wrap(Wrap { trim: false })
            .render(intro_area, buf);

        match state.step {
            ApiProviderWizardStep::WireApi => {
                let options = [ApiProviderWireApi::Chat, ApiProviderWireApi::Responses];
                let lines = options
                    .into_iter()
                    .enumerate()
                    .flat_map(|(idx, option)| {
                        let selected = option == state.wire_api;
                        let prefix = if selected { ">" } else { " " };
                        let line = if selected {
                            vec![
                                format!("{prefix} {}. ", idx + 1).cyan().dim(),
                                option.label().cyan(),
                            ]
                            .into()
                        } else {
                            format!("  {}. {}", idx + 1, option.label()).into()
                        };
                        let description = if selected {
                            format!("     {}", option.description()).cyan().dim()
                        } else {
                            format!("     {}", option.description()).dim()
                        };
                        vec![line, description.into()]
                    })
                    .collect::<Vec<Line>>();

                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .block(
                        Block::default()
                            .title(state.step.title())
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(Color::Cyan)),
                    )
                    .render(input_area, buf);
            }
            _ => {
                let value = self.current_step_value_for_display(state);
                let content_line: Line = if value.is_empty() {
                    vec![state.step.placeholder().dim()].into()
                } else {
                    value.into()
                };
                Paragraph::new(content_line)
                    .wrap(Wrap { trim: false })
                    .block(
                        Block::default()
                            .title(state.step.title())
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(Color::Cyan)),
                    )
                    .render(input_area, buf);
            }
        }

        let mut footer_lines: Vec<Line> = vec![
            if state.validating {
                "  Saving...".dim().into()
            } else if state.step == ApiProviderWizardStep::Model {
                "  Press Enter to validate and save".dim().into()
            } else {
                "  Press Enter to continue".dim().into()
            },
            if state.step == ApiProviderWizardStep::WireApi {
                "  Press 1/2 or Up/Down to choose".dim().into()
            } else {
                "  Type or paste to edit".dim().into()
            },
            "  Press Esc to go back".dim().into(),
        ];
        if let Some(error) = &state.error {
            footer_lines.push("".into());
            footer_lines.push(error.as_str().red().into());
        }
        Paragraph::new(footer_lines)
            .wrap(Wrap { trim: false })
            .render(footer_area, buf);
    }

    fn handle_api_key_entry_key_event(&mut self, key_event: &KeyEvent) -> bool {
        let mut config_to_save: Option<CustomProviderConfig> = None;
        let mut should_request_frame = false;

        {
            let mut guard = self.sign_in_state.write().unwrap();
            if let SignInState::ApiKeyEntry(state) = &mut *guard {
                if state.validating {
                    return true;
                }

                match key_event.code {
                    KeyCode::Esc => {
                        if let Some(previous_step) = state.step.previous() {
                            state.step = previous_step;
                            state.error = None;
                        } else {
                            *guard = SignInState::PickMode;
                            self.error = None;
                        }
                        should_request_frame = true;
                    }
                    KeyCode::Enter => {
                        if let Err(err) = validate_current_step(state) {
                            state.error = Some(err);
                            should_request_frame = true;
                        } else if let Some(next_step) = state.step.next() {
                            state.step = next_step;
                            state.error = None;
                            should_request_frame = true;
                        } else {
                            match snapshot_custom_provider_config(state) {
                                Ok(config) => {
                                    state.validating = true;
                                    state.error = None;
                                    config_to_save = Some(config);
                                }
                                Err(err) => {
                                    state.error = Some(err);
                                    should_request_frame = true;
                                }
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some(field) = current_step_value_mut(state) {
                            field.pop();
                            state.error = None;
                            should_request_frame = true;
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k')
                        if state.step == ApiProviderWizardStep::WireApi =>
                    {
                        state.wire_api = ApiProviderWireApi::Chat;
                        state.error = None;
                        should_request_frame = true;
                    }
                    KeyCode::Down | KeyCode::Char('j')
                        if state.step == ApiProviderWizardStep::WireApi =>
                    {
                        state.wire_api = ApiProviderWireApi::Responses;
                        state.error = None;
                        should_request_frame = true;
                    }
                    KeyCode::Left | KeyCode::Right
                        if state.step == ApiProviderWizardStep::WireApi =>
                    {
                        state.wire_api.toggle();
                        state.error = None;
                        should_request_frame = true;
                    }
                    KeyCode::Char('1') if state.step == ApiProviderWizardStep::WireApi => {
                        state.wire_api = ApiProviderWireApi::Chat;
                        state.error = None;
                        should_request_frame = true;
                    }
                    KeyCode::Char('2') if state.step == ApiProviderWizardStep::WireApi => {
                        state.wire_api = ApiProviderWireApi::Responses;
                        state.error = None;
                        should_request_frame = true;
                    }
                    KeyCode::Char(c)
                        if key_event.kind == KeyEventKind::Press
                            && !key_event.modifiers.contains(KeyModifiers::SUPER)
                            && !key_event.modifiers.contains(KeyModifiers::CONTROL)
                            && !key_event.modifiers.contains(KeyModifiers::ALT) =>
                    {
                        if state.step == ApiProviderWizardStep::WireApi {
                            match c {
                                'c' | 'C' => {
                                    state.wire_api = ApiProviderWireApi::Chat;
                                    state.error = None;
                                    should_request_frame = true;
                                }
                                'r' | 'R' => {
                                    state.wire_api = ApiProviderWireApi::Responses;
                                    state.error = None;
                                    should_request_frame = true;
                                }
                                _ => {}
                            }
                        } else if let Some(field) = current_step_value_mut(state) {
                            field.push(c);
                            state.error = None;
                            should_request_frame = true;
                        }
                    }
                    _ => {}
                }
            } else {
                return false;
            }
        }

        if let Some(config) = config_to_save {
            self.begin_custom_provider_save(config);
        } else if should_request_frame {
            self.request_frame.schedule_frame();
        }
        true
    }

    fn handle_api_key_entry_paste(&mut self, pasted: String) -> bool {
        let trimmed = pasted.trim();
        if trimmed.is_empty() {
            return false;
        }

        let mut guard = self.sign_in_state.write().unwrap();
        if let SignInState::ApiKeyEntry(state) = &mut *guard {
            if state.validating {
                return true;
            }
            if let Some(field) = current_step_value_mut(state) {
                field.push_str(trimmed);
                state.error = None;
            } else {
                return true;
            }
        } else {
            return false;
        }

        drop(guard);
        self.request_frame.schedule_frame();
        true
    }

    fn start_api_key_entry(&mut self) {
        if !self.is_api_login_allowed() {
            self.disallow_api_login();
            return;
        }
        self.error = None;
        let mut guard = self.sign_in_state.write().unwrap();
        match &mut *guard {
            SignInState::ApiKeyEntry(_) => {}
            _ => {
                *guard = SignInState::ApiKeyEntry(ApiKeyInputState::default());
            }
        }
        drop(guard);
        self.request_frame.schedule_frame();
    }

    fn begin_custom_provider_save(&mut self, config: CustomProviderConfig) {
        if !self.is_api_login_allowed() {
            self.disallow_api_login();
            return;
        }
        self.error = None;

        let sign_in_state = self.sign_in_state.clone();
        let request_frame = self.request_frame.clone();
        let codex_home = self.codex_home.clone();
        tokio::spawn(async move {
            match persist_custom_provider_config(&codex_home, &config).await {
                Ok(()) => {
                    *sign_in_state.write().unwrap() = SignInState::ApiKeyConfigured;
                }
                Err(err) => {
                    let mut guard = sign_in_state.write().unwrap();
                    if let SignInState::ApiKeyEntry(state) = &mut *guard {
                        state.validating = false;
                        state.error = Some(err);
                    } else {
                        *guard = SignInState::ApiKeyEntry(ApiKeyInputState {
                            provider_id: config.provider_id,
                            wire_api: config.wire_api,
                            base_url: config.base_url,
                            api_key: config.api_key,
                            model: config.model,
                            step: ApiProviderWizardStep::Model,
                            validating: false,
                            error: Some(err),
                        });
                    }
                }
            }
            request_frame.schedule_frame();
        });

        self.request_frame.schedule_frame();
    }

    fn current_step_value_for_display(&self, state: &ApiKeyInputState) -> String {
        match state.step {
            ApiProviderWizardStep::ProviderId => state.provider_id.clone(),
            ApiProviderWizardStep::WireApi => String::new(),
            ApiProviderWizardStep::BaseUrl => state.base_url.clone(),
            ApiProviderWizardStep::ApiKey => mask_secret(&state.api_key),
            ApiProviderWizardStep::Model => state.model.clone(),
        }
    }

    fn handle_existing_chatgpt_login(&mut self) -> bool {
        if matches!(
            self.login_status,
            LoginStatus::AuthMode(AuthMode::Chatgpt | AuthMode::ChatgptAuthTokens)
        ) {
            *self.sign_in_state.write().unwrap() = SignInState::ChatGptSuccess;
            self.request_frame.schedule_frame();
            true
        } else {
            false
        }
    }

    /// Kicks off the ChatGPT auth flow and keeps the UI state consistent with the attempt.
    fn start_chatgpt_login(&mut self) {
        // If we're already authenticated with ChatGPT, don't start a new login –
        // just proceed to the success message flow.
        if self.handle_existing_chatgpt_login() {
            return;
        }

        self.error = None;
        let opts = ServerOptions::new(
            self.codex_home.clone(),
            CLIENT_ID.to_string(),
            self.forced_chatgpt_workspace_id.clone(),
            self.cli_auth_credentials_store_mode,
        );

        match run_login_server(opts) {
            Ok(child) => {
                let sign_in_state = self.sign_in_state.clone();
                let request_frame = self.request_frame.clone();
                let auth_manager = self.auth_manager.clone();
                tokio::spawn(async move {
                    let auth_url = child.auth_url.clone();
                    {
                        *sign_in_state.write().unwrap() =
                            SignInState::ChatGptContinueInBrowser(ContinueInBrowserState {
                                auth_url,
                                shutdown_flag: Some(child.cancel_handle()),
                            });
                    }
                    request_frame.schedule_frame();
                    let r = child.block_until_done().await;
                    match r {
                        Ok(()) => {
                            // Force the auth manager to reload the new auth information.
                            auth_manager.reload();

                            *sign_in_state.write().unwrap() = SignInState::ChatGptSuccessMessage;
                            request_frame.schedule_frame();
                        }
                        _ => {
                            *sign_in_state.write().unwrap() = SignInState::PickMode;
                            // self.error = Some(e.to_string());
                            request_frame.schedule_frame();
                        }
                    }
                });
            }
            Err(e) => {
                *self.sign_in_state.write().unwrap() = SignInState::PickMode;
                self.error = Some(e.to_string());
                self.request_frame.schedule_frame();
            }
        }
    }

    #[allow(dead_code)]
    fn start_device_code_login(&mut self) {
        if self.handle_existing_chatgpt_login() {
            return;
        }

        self.error = None;
        let opts = ServerOptions::new(
            self.codex_home.clone(),
            CLIENT_ID.to_string(),
            self.forced_chatgpt_workspace_id.clone(),
            self.cli_auth_credentials_store_mode,
        );
        headless_chatgpt_login::start_headless_chatgpt_login(self, opts);
    }
}

impl StepStateProvider for AuthModeWidget {
    fn get_step_state(&self) -> StepState {
        let sign_in_state = self.sign_in_state.read().unwrap();
        match &*sign_in_state {
            SignInState::PickMode
            | SignInState::ApiKeyEntry(_)
            | SignInState::ChatGptContinueInBrowser(_)
            | SignInState::ChatGptDeviceCode(_)
            | SignInState::ChatGptSuccessMessage => StepState::InProgress,
            SignInState::ChatGptSuccess | SignInState::ApiKeyConfigured => StepState::Complete,
        }
    }
}

impl WidgetRef for AuthModeWidget {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let sign_in_state = self.sign_in_state.read().unwrap();
        match &*sign_in_state {
            SignInState::PickMode => {
                self.render_pick_mode(area, buf);
            }
            SignInState::ChatGptContinueInBrowser(_) => {
                self.render_continue_in_browser(area, buf);
            }
            SignInState::ChatGptDeviceCode(state) => {
                headless_chatgpt_login::render_device_code_login(self, area, buf, state);
            }
            SignInState::ChatGptSuccessMessage => {
                self.render_chatgpt_success_message(area, buf);
            }
            SignInState::ChatGptSuccess => {
                self.render_chatgpt_success(area, buf);
            }
            SignInState::ApiKeyEntry(state) => {
                self.render_api_key_entry(area, buf, state);
            }
            SignInState::ApiKeyConfigured => {
                self.render_api_key_configured(area, buf);
            }
        }
    }
}

fn current_step_value_mut(state: &mut ApiKeyInputState) -> Option<&mut String> {
    match state.step {
        ApiProviderWizardStep::ProviderId => Some(&mut state.provider_id),
        ApiProviderWizardStep::WireApi => None,
        ApiProviderWizardStep::BaseUrl => Some(&mut state.base_url),
        ApiProviderWizardStep::ApiKey => Some(&mut state.api_key),
        ApiProviderWizardStep::Model => Some(&mut state.model),
    }
}

fn validate_current_step(state: &ApiKeyInputState) -> Result<(), String> {
    match state.step {
        ApiProviderWizardStep::ProviderId => validate_provider_id(state.provider_id.trim()),
        ApiProviderWizardStep::WireApi => Ok(()),
        ApiProviderWizardStep::BaseUrl => validate_base_url(state.base_url.trim()),
        ApiProviderWizardStep::ApiKey => validate_non_empty(state.api_key.trim(), "API key"),
        ApiProviderWizardStep::Model => validate_non_empty(state.model.trim(), "Model"),
    }
}

fn validate_non_empty(value: &str, field_name: &str) -> Result<(), String> {
    if value.is_empty() {
        Err(format!("{field_name} cannot be empty"))
    } else {
        Ok(())
    }
}

fn validate_provider_id(provider_id: &str) -> Result<(), String> {
    validate_non_empty(provider_id, "Provider ID")?;
    if provider_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        Ok(())
    } else {
        Err("Provider ID can only contain letters, numbers, '_' or '-'".to_string())
    }
}

fn validate_base_url(base_url: &str) -> Result<(), String> {
    validate_non_empty(base_url, "Base URL")?;
    let url = url::Url::parse(base_url).map_err(|err| format!("Invalid base URL: {err}"))?;
    match url.scheme() {
        "http" | "https" => Ok(()),
        scheme => Err(format!("Base URL must use http or https, got {scheme}")),
    }
}

fn snapshot_custom_provider_config(
    state: &ApiKeyInputState,
) -> Result<CustomProviderConfig, String> {
    let provider_id = state.provider_id.trim().to_string();
    let base_url = state.base_url.trim().to_string();
    let api_key = state.api_key.trim().to_string();
    let model = state.model.trim().to_string();

    validate_provider_id(&provider_id)?;
    validate_base_url(&base_url)?;
    validate_non_empty(&api_key, "API key")?;
    validate_non_empty(&model, "Model")?;

    Ok(CustomProviderConfig {
        provider_id,
        wire_api: state.wire_api,
        base_url,
        api_key,
        model,
    })
}

async fn persist_custom_provider_config(
    codex_home: &Path,
    config: &CustomProviderConfig,
) -> Result<(), String> {
    validate_custom_provider_config(config).await?;

    ConfigEditsBuilder::new(codex_home)
        .with_edits(build_custom_provider_edits(config))
        .apply()
        .await
        .map_err(|err| format!("Failed to write config.toml: {err}"))
}

async fn validate_custom_provider_config(config: &CustomProviderConfig) -> Result<(), String> {
    let provider = Provider {
        name: config.provider_id.clone(),
        base_url: config.base_url.clone(),
        query_params: None,
        wire: config.wire_api.as_api_wire(),
        headers: HeaderMap::new(),
        retry: RetryConfig {
            max_attempts: 1,
            base_delay: Duration::from_millis(200),
            retry_429: false,
            retry_5xx: false,
            retry_transport: false,
        },
        stream_idle_timeout: Duration::from_secs(5),
    };
    let client = ModelsClient::new(
        ReqwestTransport::new(build_reqwest_client()),
        provider,
        StaticBearerAuth {
            api_key: config.api_key.clone(),
        },
    );

    match client
        .list_models(env!("CARGO_PKG_VERSION"), HeaderMap::new())
        .await
    {
        Ok(_) => Ok(()),
        Err(ApiError::Transport(TransportError::Http {
            status:
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED,
            ..
        })) => Ok(()),
        Err(ApiError::Transport(TransportError::Http {
            status: StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN,
            ..
        })) => Err(
            "Authentication failed while checking the provider. Verify the API key.".to_string(),
        ),
        Err(ApiError::Transport(TransportError::Timeout)) => {
            Err("Timed out while connecting to the provider.".to_string())
        }
        Err(ApiError::Transport(TransportError::RetryLimit)) => {
            Err("Could not connect to the provider after retrying.".to_string())
        }
        Err(ApiError::Transport(TransportError::Build(err)))
        | Err(ApiError::Transport(TransportError::Network(err))) => {
            Err(format!("Could not connect to the provider: {err}"))
        }
        Err(ApiError::Transport(TransportError::Http { .. })) | Err(ApiError::Stream(_)) => Ok(()),
        Err(ApiError::Api { status, message }) => {
            if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
                Err(format!(
                    "Authentication failed while checking the provider: {message}"
                ))
            } else {
                Ok(())
            }
        }
        Err(
            ApiError::ContextWindowExceeded
            | ApiError::QuotaExceeded
            | ApiError::UsageNotIncluded
            | ApiError::Retryable { .. }
            | ApiError::RateLimit(_)
            | ApiError::InvalidRequest { .. },
        ) => Ok(()),
    }
}

fn build_custom_provider_edits(config: &CustomProviderConfig) -> Vec<ConfigEdit> {
    let provider_path = vec!["model_providers".to_string(), config.provider_id.clone()];

    vec![
        ConfigEdit::ClearPath {
            segments: provider_path,
        },
        ConfigEdit::SetPath {
            segments: vec!["model_provider".to_string()],
            value: value(config.provider_id.clone()),
        },
        ConfigEdit::SetPath {
            segments: vec!["model".to_string()],
            value: value(config.model.clone()),
        },
        ConfigEdit::SetPath {
            segments: vec![
                "model_providers".to_string(),
                config.provider_id.clone(),
                "name".to_string(),
            ],
            value: value(config.provider_id.clone()),
        },
        ConfigEdit::SetPath {
            segments: vec![
                "model_providers".to_string(),
                config.provider_id.clone(),
                "base_url".to_string(),
            ],
            value: value(config.base_url.clone()),
        },
        ConfigEdit::SetPath {
            segments: vec![
                "model_providers".to_string(),
                config.provider_id.clone(),
                "wire_api".to_string(),
            ],
            value: value(config.wire_api.as_config_value()),
        },
        ConfigEdit::SetPath {
            segments: vec![
                "model_providers".to_string(),
                config.provider_id.clone(),
                "experimental_bearer_token".to_string(),
            ],
            value: value(config.api_key.clone()),
        },
        ConfigEdit::SetPath {
            segments: vec![
                "model_providers".to_string(),
                config.provider_id.clone(),
                "requires_openai_auth".to_string(),
            ],
            value: value(false),
        },
    ]
}

fn mask_secret(value: &str) -> String {
    if value.is_empty() {
        "<not set>".to_string()
    } else {
        "*".repeat(value.chars().count().min(32))
    }
}

fn display_optional_value<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use tempfile::TempDir;

    use codex_core::auth::AuthCredentialsStoreMode;
    use codex_core::config::CONFIG_TOML_FILE;

    fn widget_forced_chatgpt() -> (AuthModeWidget, TempDir) {
        let codex_home = TempDir::new().unwrap();
        let codex_home_path = codex_home.path().to_path_buf();
        let widget = AuthModeWidget {
            request_frame: FrameRequester::test_dummy(),
            highlighted_mode: SignInOption::ChatGpt,
            error: None,
            sign_in_state: Arc::new(RwLock::new(SignInState::PickMode)),
            codex_home: codex_home_path.clone(),
            cli_auth_credentials_store_mode: AuthCredentialsStoreMode::File,
            login_status: LoginStatus::NotAuthenticated,
            auth_manager: AuthManager::shared(
                codex_home_path,
                false,
                AuthCredentialsStoreMode::File,
            ),
            forced_chatgpt_workspace_id: None,
            forced_login_method: Some(ForcedLoginMethod::Chatgpt),
            animations_enabled: true,
        };
        (widget, codex_home)
    }

    fn widget_custom_provider_entry() -> (AuthModeWidget, TempDir) {
        let codex_home = TempDir::new().unwrap();
        let codex_home_path = codex_home.path().to_path_buf();
        let widget = AuthModeWidget {
            request_frame: FrameRequester::test_dummy(),
            highlighted_mode: SignInOption::ApiKey,
            error: None,
            sign_in_state: Arc::new(RwLock::new(SignInState::ApiKeyEntry(
                ApiKeyInputState::default(),
            ))),
            codex_home: codex_home_path.clone(),
            cli_auth_credentials_store_mode: AuthCredentialsStoreMode::File,
            login_status: LoginStatus::NotAuthenticated,
            auth_manager: AuthManager::shared(
                codex_home_path,
                false,
                AuthCredentialsStoreMode::File,
            ),
            forced_chatgpt_workspace_id: None,
            forced_login_method: None,
            animations_enabled: true,
        };
        (widget, codex_home)
    }

    fn row_text(buf: &Buffer, row: u16, width: u16) -> String {
        (0..width)
            .map(|col| buf[(col, row)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn api_key_flow_disabled_when_chatgpt_forced() {
        let (mut widget, _tmp) = widget_forced_chatgpt();

        widget.start_api_key_entry();

        assert_eq!(widget.error.as_deref(), Some(API_KEY_DISABLED_MESSAGE));
        assert!(matches!(
            &*widget.sign_in_state.read().unwrap(),
            SignInState::PickMode
        ));
    }

    #[test]
    fn saving_api_key_is_blocked_when_chatgpt_forced() {
        let (mut widget, _tmp) = widget_forced_chatgpt();

        widget.begin_custom_provider_save(CustomProviderConfig {
            provider_id: "test".to_string(),
            wire_api: ApiProviderWireApi::Responses,
            base_url: "https://example.com/v1".to_string(),
            api_key: "sk-test".to_string(),
            model: "gpt-test".to_string(),
        });

        assert_eq!(widget.error.as_deref(), Some(API_KEY_DISABLED_MESSAGE));
        assert!(matches!(
            &*widget.sign_in_state.read().unwrap(),
            SignInState::PickMode
        ));
        assert_eq!(widget.login_status, LoginStatus::NotAuthenticated);
    }

    #[test]
    fn provider_id_validation_rejects_invalid_characters() {
        assert_eq!(
            validate_provider_id("my provider").unwrap_err(),
            "Provider ID can only contain letters, numbers, '_' or '-'"
        );
        assert!(validate_provider_id("custom_1").is_ok());
    }

    #[test]
    fn snapshot_custom_provider_config_trims_values() {
        let state = ApiKeyInputState {
            provider_id: " custom_1 ".to_string(),
            wire_api: ApiProviderWireApi::Chat,
            base_url: " https://example.com/v1/ ".to_string(),
            api_key: " secret ".to_string(),
            model: " model-x ".to_string(),
            step: ApiProviderWizardStep::Model,
            validating: false,
            error: None,
        };

        let config = snapshot_custom_provider_config(&state).unwrap();

        assert_eq!(
            config,
            CustomProviderConfig {
                provider_id: "custom_1".to_string(),
                wire_api: ApiProviderWireApi::Chat,
                base_url: "https://example.com/v1/".to_string(),
                api_key: "secret".to_string(),
                model: "model-x".to_string(),
            }
        );
    }

    #[test]
    fn custom_provider_entry_keeps_blank_line_before_input_box() {
        let (widget, _tmp) = widget_custom_provider_entry();
        let area = Rect::new(0, 0, 160, 20);
        let mut buf = Buffer::empty(area);

        (&widget).render_ref(area, &mut buf);

        let lines = (0..area.height)
            .map(|row| row_text(&buf, row, area.width))
            .collect::<Vec<String>>();

        let model_row = lines
            .iter()
            .position(|line| line.contains("  Model: <not set>"))
            .expect("model summary row");
        let border_row = lines
            .iter()
            .position(|line| line.starts_with("╭Provider ID"))
            .expect("input border row");

        assert_eq!(border_row, model_row + 2);
        assert_eq!(lines[model_row + 1], "");
    }

    #[test]
    fn wire_api_entry_uses_dynamic_input_height() {
        let (widget, _tmp) = widget_custom_provider_entry();
        let state = ApiKeyInputState {
            step: ApiProviderWizardStep::WireApi,
            ..ApiKeyInputState::default()
        };

        let input_height = widget.api_key_entry_input_height(120, &state);

        assert_eq!(input_height, 6);
    }

    #[test]
    fn custom_provider_edits_write_expected_config() {
        let codex_home = TempDir::new().unwrap();
        let config = CustomProviderConfig {
            provider_id: "custom_1".to_string(),
            wire_api: ApiProviderWireApi::Responses,
            base_url: "https://example.com/v1".to_string(),
            api_key: "sk-test".to_string(),
            model: "gpt-test".to_string(),
        };

        ConfigEditsBuilder::new(codex_home.path())
            .with_edits(build_custom_provider_edits(&config))
            .apply_blocking()
            .unwrap();

        let raw = std::fs::read_to_string(codex_home.path().join(CONFIG_TOML_FILE)).unwrap();
        assert_eq!(
            raw,
            r#"model_provider = "custom_1"
model = "gpt-test"

[model_providers.custom_1]
name = "custom_1"
base_url = "https://example.com/v1"
wire_api = "responses"
experimental_bearer_token = "sk-test"
requires_openai_auth = false
"#
        );
    }
}
