use std::collections::HashSet;

const CONTEXT_WINDOW_STEPS: [i64; 3] = [32_000, 128_000, 200_000];

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DynamicContextWindowKey {
    pub(crate) model_provider_id: String,
    pub(crate) model: String,
}

impl DynamicContextWindowKey {
    pub(crate) fn new(model_provider_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            model_provider_id: model_provider_id.into(),
            model: model.into(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct DynamicContextWindowState {
    current_step_index: usize,
    retry_state: RetryState,
}

#[derive(Debug, Default)]
struct RetryState {
    turn_id: Option<String>,
    retried_windows: HashSet<i64>,
}

impl DynamicContextWindowState {
    pub(crate) fn new() -> Self {
        Self {
            current_step_index: 0,
            retry_state: RetryState::default(),
        }
    }

    pub(crate) fn current_context_window(&self) -> i64 {
        CONTEXT_WINDOW_STEPS[self.current_step_index]
    }

    fn effective_context_window(&self, effective_context_window_percent: i64) -> i64 {
        self.current_context_window()
            .saturating_mul(effective_context_window_percent)
            / 100
    }

    pub(crate) fn maybe_upgrade(
        &mut self,
        input_tokens: i64,
        effective_context_window_percent: i64,
    ) -> Option<i64> {
        let current = self.effective_context_window(effective_context_window_percent);
        if input_tokens <= current || self.current_step_index + 1 >= CONTEXT_WINDOW_STEPS.len() {
            return None;
        }

        self.current_step_index += 1;
        Some(self.current_context_window())
    }

    pub(crate) fn record_compact_retry(
        &mut self,
        turn_id: &str,
        input_tokens: i64,
        effective_context_window_percent: i64,
    ) -> bool {
        let current = self.effective_context_window(effective_context_window_percent);
        if input_tokens <= current {
            return false;
        }

        if self.retry_state.turn_id.as_deref() != Some(turn_id) {
            self.retry_state.turn_id = Some(turn_id.to_string());
            self.retry_state.retried_windows.clear();
        }

        self.retry_state.retried_windows.insert(current)
    }
}

#[cfg(test)]
mod tests {
    use super::DynamicContextWindowState;
    use pretty_assertions::assert_eq;

    #[test]
    fn upgrades_through_supported_steps() {
        let mut state = DynamicContextWindowState::new();

        assert_eq!(state.current_context_window(), 32_000);
        assert_eq!(state.maybe_upgrade(30_400, 95), None);
        assert_eq!(state.maybe_upgrade(30_401, 95), Some(128_000));
        assert_eq!(state.current_context_window(), 128_000);
        assert_eq!(state.maybe_upgrade(121_600, 95), None);
        assert_eq!(state.maybe_upgrade(121_601, 95), Some(200_000));
        assert_eq!(state.current_context_window(), 200_000);
        assert_eq!(state.maybe_upgrade(190_001, 95), None);
    }

    #[test]
    fn compact_retry_is_limited_per_turn_and_step() {
        let mut state = DynamicContextWindowState::new();

        assert!(state.record_compact_retry("turn-1", 40_000, 95));
        assert!(!state.record_compact_retry("turn-1", 40_000, 95));

        assert_eq!(state.maybe_upgrade(40_000, 95), Some(128_000));
        assert!(state.record_compact_retry("turn-1", 140_000, 95));
        assert!(!state.record_compact_retry("turn-1", 140_000, 95));

        assert!(state.record_compact_retry("turn-2", 140_000, 95));
    }
}
