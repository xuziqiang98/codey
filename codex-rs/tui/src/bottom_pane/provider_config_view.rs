use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::sync::RwLockReadGuard;
use std::sync::RwLockWriteGuard;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Block;
use ratatui::widgets::BorderType;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use ratatui::widgets::StatefulWidgetRef;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::provider_config::ApiKeyInputState;
use crate::provider_config::ApiProviderWireApi;
use crate::provider_config::ApiProviderWizardStep;
use crate::provider_config::CustomProviderConfig;
use crate::provider_config::current_step_value_mut;
use crate::provider_config::persist_custom_provider_config;
use crate::provider_config::snapshot_custom_provider_config;
use crate::provider_config::validate_current_step;
use crate::render::renderable::Renderable;
use crate::tui::FrameRequester;

use super::CancellationEvent;
use super::bottom_pane_view::BottomPaneView;
use super::popup_consts::standard_popup_hint_line;
use super::textarea::TextArea;
use super::textarea::TextAreaState;

pub(crate) struct ProviderConfigView {
    state: Arc<RwLock<ApiKeyInputState>>,
    textarea: TextArea,
    textarea_state: RefCell<TextAreaState>,
    complete: bool,
    codex_home: PathBuf,
    app_event_tx: AppEventSender,
    request_frame: FrameRequester,
}

impl ProviderConfigView {
    pub(crate) fn new(
        codex_home: PathBuf,
        app_event_tx: AppEventSender,
        request_frame: FrameRequester,
    ) -> Self {
        let mut textarea = TextArea::new();
        textarea.set_text_clearing_elements("");
        Self {
            state: Arc::new(RwLock::new(ApiKeyInputState::default())),
            textarea,
            textarea_state: RefCell::new(TextAreaState::default()),
            complete: false,
            codex_home,
            app_event_tx,
            request_frame,
        }
    }

    fn sync_textarea_from_state(&mut self) {
        let state = read_state(&self.state);
        let text = match state.step {
            ApiProviderWizardStep::ProviderId => state.provider_id.clone(),
            ApiProviderWizardStep::WireApi => String::new(),
            ApiProviderWizardStep::BaseUrl => state.base_url.clone(),
            ApiProviderWizardStep::ApiKey => state.api_key.clone(),
            ApiProviderWizardStep::Model => state.model.clone(),
        };
        drop(state);
        self.textarea.set_text_clearing_elements(&text);
        self.textarea_state.replace(TextAreaState::default());
    }

    fn update_state_from_textarea(&self, state: &mut ApiKeyInputState) {
        if let Some(field) = current_step_value_mut(state) {
            *field = self.textarea.text().to_string();
        }
    }

    fn start_save(&mut self, config: CustomProviderConfig) {
        {
            let mut state = write_state(&self.state);
            state.validating = true;
            state.error = None;
        }

        let state = Arc::clone(&self.state);
        let codex_home = self.codex_home.clone();
        let app_event_tx = self.app_event_tx.clone();
        let request_frame = self.request_frame.clone();
        tokio::spawn(async move {
            match persist_custom_provider_config(&codex_home, &config).await {
                Ok(()) => {
                    app_event_tx.send(AppEvent::CustomProviderConfigured(config));
                }
                Err(err) => {
                    let mut state = state.write().unwrap_or_else(PoisonError::into_inner);
                    state.validating = false;
                    state.error = Some(err);
                    request_frame.schedule_frame();
                }
            }
        });
    }

    fn input_height(&self, area: Rect, state: &ApiKeyInputState) -> u16 {
        let inner_width = area.width.saturating_sub(2).max(1);
        match state.step {
            ApiProviderWizardStep::WireApi => 6,
            _ => self
                .textarea
                .desired_height(inner_width)
                .clamp(1, 4)
                .saturating_add(2),
        }
    }
}

fn read_state(state: &RwLock<ApiKeyInputState>) -> RwLockReadGuard<'_, ApiKeyInputState> {
    state.read().unwrap_or_else(PoisonError::into_inner)
}

fn write_state(state: &RwLock<ApiKeyInputState>) -> RwLockWriteGuard<'_, ApiKeyInputState> {
    state.write().unwrap_or_else(PoisonError::into_inner)
}

impl BottomPaneView for ProviderConfigView {
    fn handle_key_event(&mut self, key_event: KeyEvent) {
        let mut config_to_save: Option<CustomProviderConfig> = None;
        let mut should_complete = false;
        let mut should_sync_textarea = false;
        let mut should_request_frame = false;

        {
            let mut state = write_state(&self.state);
            if state.validating {
                return;
            }

            self.update_state_from_textarea(&mut state);

            match key_event.code {
                KeyCode::Esc => {
                    if let Some(previous_step) = state.step.previous() {
                        state.step = previous_step;
                        state.error = None;
                        should_sync_textarea = true;
                    } else {
                        should_complete = true;
                    }
                    should_request_frame = true;
                }
                KeyCode::Enter => {
                    if let Err(err) = validate_current_step(&state) {
                        state.error = Some(err);
                        should_request_frame = true;
                    } else if let Some(next_step) = state.step.next() {
                        state.step = next_step;
                        state.error = None;
                        should_sync_textarea = true;
                        should_request_frame = true;
                    } else {
                        match snapshot_custom_provider_config(&state) {
                            Ok(config) => config_to_save = Some(config),
                            Err(err) => {
                                state.error = Some(err);
                                should_request_frame = true;
                            }
                        }
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
                KeyCode::Left | KeyCode::Right if state.step == ApiProviderWizardStep::WireApi => {
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
                KeyCode::Char('c') | KeyCode::Char('C')
                    if state.step == ApiProviderWizardStep::WireApi =>
                {
                    state.wire_api = ApiProviderWireApi::Chat;
                    state.error = None;
                    should_request_frame = true;
                }
                KeyCode::Char('r') | KeyCode::Char('R')
                    if state.step == ApiProviderWizardStep::WireApi =>
                {
                    state.wire_api = ApiProviderWireApi::Responses;
                    state.error = None;
                    should_request_frame = true;
                }
                _ if state.step != ApiProviderWizardStep::WireApi => {
                    if key_event
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SUPER)
                    {
                        return;
                    }
                    self.textarea.input(key_event);
                    self.update_state_from_textarea(&mut state);
                    state.error = None;
                    should_request_frame = true;
                }
                _ => {}
            }
        }

        if should_sync_textarea {
            self.sync_textarea_from_state();
        }
        if should_complete {
            self.complete = true;
        }
        if let Some(config) = config_to_save {
            self.start_save(config);
            self.request_frame.schedule_frame();
        } else if should_request_frame {
            self.request_frame.schedule_frame();
        }
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.complete = true;
        CancellationEvent::Handled
    }

    fn is_complete(&self) -> bool {
        self.complete
    }

    fn handle_paste(&mut self, pasted: String) -> bool {
        let pasted = pasted.trim();
        if pasted.is_empty() {
            return false;
        }

        let mut state = write_state(&self.state);
        if state.validating || state.step == ApiProviderWizardStep::WireApi {
            return true;
        }

        self.textarea.set_text_clearing_elements(pasted);
        self.textarea_state.replace(TextAreaState::default());
        self.update_state_from_textarea(&mut state);
        state.error = None;
        drop(state);
        self.request_frame.schedule_frame();
        true
    }
}

impl Renderable for ProviderConfigView {
    fn desired_height(&self, width: u16) -> u16 {
        let state = read_state(&self.state);
        let input_height = self.input_height(
            Rect {
                x: 0,
                y: 0,
                width,
                height: 0,
            },
            &state,
        );
        let intro_min_height = if state.validating { 12 } else { 10 };
        intro_min_height + input_height + 5
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let state = read_state(&self.state).clone();
        let input_height = self.input_height(area, &state);
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
            "  This saves the provider and model to ~/.codey/config.toml.".into(),
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
                Block::default()
                    .title(state.step.title())
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Cyan))
                    .render(input_area, buf);
                let textarea_rect = Rect {
                    x: input_area.x.saturating_add(1),
                    y: input_area.y.saturating_add(1),
                    width: input_area.width.saturating_sub(2),
                    height: input_area.height.saturating_sub(2),
                };
                StatefulWidgetRef::render_ref(
                    &(&self.textarea),
                    textarea_rect,
                    buf,
                    &mut self.textarea_state.borrow_mut(),
                );
                if self.textarea.text().is_empty() {
                    Paragraph::new(Line::from(state.step.placeholder().dim()))
                        .render(textarea_rect, buf);
                }
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
            standard_popup_hint_line(),
        ];
        if let Some(error) = &state.error {
            footer_lines.push("".into());
            footer_lines.push(error.as_str().red().into());
        }
        Paragraph::new(footer_lines)
            .wrap(Wrap { trim: false })
            .render(footer_area, buf);
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let state = read_state(&self.state);
        if state.step == ApiProviderWizardStep::WireApi || state.validating {
            return None;
        }

        let input_height = self.input_height(area, &state);
        let intro_min_height = if state.validating { 12 } else { 10 };
        let textarea_rect = Rect {
            x: area.x.saturating_add(1),
            y: area.y.saturating_add(intro_min_height).saturating_add(2),
            width: area.width.saturating_sub(2),
            height: input_height.saturating_sub(2),
        };
        let textarea_state = *self.textarea_state.borrow();
        self.textarea
            .cursor_pos_with_state(textarea_rect, textarea_state)
    }
}

fn display_optional_value(value: &str, placeholder: &str) -> String {
    if value.trim().is_empty() {
        placeholder.to_string()
    } else {
        value.to_string()
    }
}

fn mask_secret(value: &str) -> String {
    if value.trim().is_empty() {
        "<not set>".to_string()
    } else {
        "********".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    fn test_view() -> ProviderConfigView {
        let (tx, _rx) = unbounded_channel();
        ProviderConfigView::new(
            PathBuf::from("/tmp"),
            AppEventSender::new(tx),
            FrameRequester::test_dummy(),
        )
    }

    fn state_snapshot(view: &ProviderConfigView) -> ApiKeyInputState {
        view.state
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    #[test]
    fn esc_on_first_step_cancels_view() {
        let mut view = test_view();

        view.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(view.is_complete());
    }

    #[test]
    fn enter_and_esc_follow_wizard_steps() {
        let mut view = test_view();
        assert!(view.handle_paste("custom_1".to_string()));

        view.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(state_snapshot(&view).step, ApiProviderWizardStep::WireApi);

        view.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(state_snapshot(&view).step, ApiProviderWizardStep::BaseUrl);

        view.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(state_snapshot(&view).step, ApiProviderWizardStep::WireApi);
    }

    #[test]
    fn paste_and_wire_api_shortcuts_update_state() {
        let mut view = test_view();

        assert!(view.handle_paste(" custom_1 ".to_string()));
        assert_eq!(state_snapshot(&view).provider_id, "custom_1");

        view.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        view.handle_key_event(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert_eq!(state_snapshot(&view).wire_api, ApiProviderWireApi::Chat);

        view.handle_key_event(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(
            state_snapshot(&view).wire_api,
            ApiProviderWireApi::Responses
        );
    }
}
