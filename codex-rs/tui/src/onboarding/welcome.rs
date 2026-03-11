use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::prelude::Widget;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::WidgetRef;
use ratatui::widgets::Wrap;
use std::cell::Cell;

use crate::onboarding::onboarding_screen::KeyboardHandler;
use crate::onboarding::onboarding_screen::StepStateProvider;
use crate::tui::FrameRequester;

use super::onboarding_screen::StepState;

const CODEY_ASCII: [&str; 5] = [
    " ######   #####   ######   #######  ##   ##",
    "###      ##   ##  ##   ##  ##       ##   ##",
    "##       ##   ##  ##   ##  ######    #####",
    "###      ##   ##  ##   ##  ##          ##",
    " ######   #####   ######   #######     ##",
];
const ASCII_ART_HEIGHT: u16 = 5;
const ASCII_ART_WIDTH: u16 = 43;

pub(crate) struct WelcomeWidget {
    pub is_logged_in: bool,
    layout_area: Cell<Option<Rect>>,
}

impl KeyboardHandler for WelcomeWidget {
    fn handle_key_event(&mut self, _key_event: KeyEvent) {}
}

impl WelcomeWidget {
    pub(crate) fn new(
        is_logged_in: bool,
        _request_frame: FrameRequester,
        _animations_enabled: bool,
    ) -> Self {
        Self {
            is_logged_in,
            layout_area: Cell::new(None),
        }
    }

    pub(crate) fn update_layout_area(&self, area: Rect) {
        self.layout_area.set(Some(area));
    }
}

impl WidgetRef for &WelcomeWidget {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);
        let layout_area = self.layout_area.get().unwrap_or(area);
        // Skip the ASCII art when the viewport is too small so we don't clip it.
        let show_ascii_art =
            layout_area.height >= ASCII_ART_HEIGHT + 2 && layout_area.width >= ASCII_ART_WIDTH;

        let mut lines: Vec<Line> = Vec::new();
        if show_ascii_art {
            lines.extend(CODEY_ASCII.into_iter().map(Into::into));
            lines.push("".into());
        }
        lines.push(Line::from(vec![
            "  ".into(),
            "Welcome to ".into(),
            "Codey".bold(),
            ", a lightweight command-line coding agent".into(),
        ]));

        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }
}

impl StepStateProvider for WelcomeWidget {
    fn get_step_state(&self) -> StepState {
        match self.is_logged_in {
            true => StepState::Hidden,
            false => StepState::Complete,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn row_containing(buf: &Buffer, needle: &str) -> Option<u16> {
        (0..buf.area.height).find(|&y| {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            row.contains(needle)
        })
    }

    #[test]
    fn welcome_renders_codey_ascii_on_first_draw() {
        let widget = WelcomeWidget::new(false, FrameRequester::test_dummy(), true);
        let area = Rect::new(0, 0, ASCII_ART_WIDTH, ASCII_ART_HEIGHT + 2);
        let mut buf = Buffer::empty(area);
        (&widget).render(area, &mut buf);

        let ascii_row = row_containing(&buf, "#######");
        assert_eq!(ascii_row, Some(0));

        let welcome_row = row_containing(&buf, "Welcome");
        assert_eq!(welcome_row, Some(ASCII_ART_HEIGHT + 1));
    }

    #[test]
    fn welcome_skips_codey_ascii_below_height_breakpoint() {
        let widget = WelcomeWidget::new(false, FrameRequester::test_dummy(), true);
        let area = Rect::new(0, 0, ASCII_ART_WIDTH, ASCII_ART_HEIGHT + 1);
        let mut buf = Buffer::empty(area);
        (&widget).render(area, &mut buf);

        let ascii_row = row_containing(&buf, "#######");
        assert_eq!(ascii_row, None);

        let welcome_row = row_containing(&buf, "Welcome");
        assert_eq!(welcome_row, Some(0));
    }
}
