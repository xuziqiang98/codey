use std::collections::HashSet;

const CONTEXT_WINDOW_STEPS: [i64; 4] = [32_000, 64_000, 128_000, 200_000];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DynamicContextWindowSuccess {
    pub(crate) context_window: i64,
    pub(crate) learned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DynamicContextWindowFailure {
    pub(crate) should_retry: bool,
    pub(crate) learned_context_window: Option<i64>,
}

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
    locked_step_index: Option<usize>,
    last_successful_step_index: Option<usize>,
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
            locked_step_index: None,
            last_successful_step_index: None,
            retry_state: RetryState::default(),
        }
    }

    pub(crate) fn current_context_window(&self) -> i64 {
        let index = self.locked_step_index.unwrap_or(self.current_step_index);
        CONTEXT_WINDOW_STEPS[index]
    }

    fn effective_context_window(&self, effective_context_window_percent: i64) -> i64 {
        self.current_context_window()
            .saturating_mul(effective_context_window_percent)
            / 100
    }

    pub(crate) fn is_locked(&self) -> bool {
        self.locked_step_index.is_some()
    }

    fn has_next_step(&self) -> bool {
        self.current_step_index + 1 < CONTEXT_WINDOW_STEPS.len()
    }

    pub(crate) fn should_probe_optimistically(
        &self,
        input_tokens: i64,
        effective_context_window_percent: i64,
    ) -> bool {
        !self.is_locked()
            && input_tokens >= self.effective_context_window(effective_context_window_percent)
    }

    pub(crate) fn should_preflight_compact(
        &self,
        input_tokens: i64,
        effective_context_window_percent: i64,
    ) -> bool {
        self.is_locked()
            && input_tokens >= self.effective_context_window(effective_context_window_percent)
    }

    pub(crate) fn record_success(
        &mut self,
        input_tokens: i64,
        effective_context_window_percent: i64,
    ) -> Option<DynamicContextWindowSuccess> {
        if !self.should_probe_optimistically(input_tokens, effective_context_window_percent) {
            return None;
        }

        if self.has_next_step() {
            self.last_successful_step_index = Some(self.current_step_index);
            self.current_step_index += 1;
            Some(DynamicContextWindowSuccess {
                context_window: self.current_context_window(),
                learned: false,
            })
        } else {
            self.last_successful_step_index = Some(self.current_step_index);
            self.locked_step_index = Some(self.current_step_index);
            Some(DynamicContextWindowSuccess {
                context_window: self.current_context_window(),
                learned: true,
            })
        }
    }

    pub(crate) fn record_probe_failure(
        &mut self,
        turn_id: &str,
        input_tokens: i64,
        effective_context_window_percent: i64,
    ) -> DynamicContextWindowFailure {
        let current = self.effective_context_window(effective_context_window_percent);
        if input_tokens < current {
            return DynamicContextWindowFailure {
                should_retry: false,
                learned_context_window: None,
            };
        }

        if self.retry_state.turn_id.as_deref() != Some(turn_id) {
            self.retry_state.turn_id = Some(turn_id.to_string());
            self.retry_state.retried_windows.clear();
        }

        let learned_step_index = self
            .locked_step_index
            .or(self.last_successful_step_index)
            .unwrap_or(self.current_step_index);
        let retry_window = CONTEXT_WINDOW_STEPS[learned_step_index]
            .saturating_mul(effective_context_window_percent)
            / 100;
        let should_retry = self.retry_state.retried_windows.insert(retry_window);
        if !should_retry {
            return DynamicContextWindowFailure {
                should_retry: false,
                learned_context_window: self
                    .locked_step_index
                    .map(|_| self.current_context_window()),
            };
        }

        self.current_step_index = learned_step_index;
        self.locked_step_index = Some(learned_step_index);

        DynamicContextWindowFailure {
            should_retry,
            learned_context_window: Some(self.current_context_window()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DynamicContextWindowFailure;
    use super::DynamicContextWindowState;
    use super::DynamicContextWindowSuccess;
    use pretty_assertions::assert_eq;

    #[test]
    fn upgrades_through_supported_steps() {
        let mut state = DynamicContextWindowState::new();

        assert_eq!(state.current_context_window(), 32_000);
        assert!(state.should_probe_optimistically(30_400, 95));
        assert_eq!(
            state.record_success(30_400, 95),
            Some(DynamicContextWindowSuccess {
                context_window: 64_000,
                learned: false,
            })
        );
        assert_eq!(state.current_context_window(), 64_000);
        assert!(state.should_probe_optimistically(60_800, 95));
        assert_eq!(
            state.record_success(60_800, 95),
            Some(DynamicContextWindowSuccess {
                context_window: 128_000,
                learned: false,
            })
        );
        assert_eq!(state.current_context_window(), 128_000);
        assert!(state.should_probe_optimistically(121_600, 95));
        assert_eq!(
            state.record_success(121_600, 95),
            Some(DynamicContextWindowSuccess {
                context_window: 200_000,
                learned: false,
            })
        );
        assert_eq!(state.current_context_window(), 200_000);
        assert!(state.should_probe_optimistically(190_001, 95));
        assert_eq!(
            state.record_success(190_001, 95),
            Some(DynamicContextWindowSuccess {
                context_window: 200_000,
                learned: true,
            })
        );
        assert!(state.is_locked());
    }

    #[test]
    fn probe_failure_locks_to_last_successful_step() {
        let mut state = DynamicContextWindowState::new();

        let _ = state.record_success(30_400, 95);
        assert_eq!(
            state.record_probe_failure("turn-1", 80_000, 95),
            DynamicContextWindowFailure {
                should_retry: true,
                learned_context_window: Some(32_000),
            }
        );
        assert!(state.is_locked());
        assert_eq!(state.current_context_window(), 32_000);
        assert!(state.should_preflight_compact(40_000, 95));
        assert!(!state.should_probe_optimistically(40_000, 95));
    }

    #[test]
    fn first_failure_locks_to_minimum_step() {
        let mut state = DynamicContextWindowState::new();

        assert_eq!(
            state.record_probe_failure("turn-1", 40_000, 95),
            DynamicContextWindowFailure {
                should_retry: true,
                learned_context_window: Some(32_000),
            }
        );
        assert!(state.is_locked());
        assert!(state.should_preflight_compact(40_000, 95));
    }

    #[test]
    fn compact_retry_is_limited_per_turn_and_step() {
        let mut state = DynamicContextWindowState::new();

        assert_eq!(
            state.record_probe_failure("turn-1", 40_000, 95),
            DynamicContextWindowFailure {
                should_retry: true,
                learned_context_window: Some(32_000),
            }
        );
        assert_eq!(
            state.record_probe_failure("turn-1", 40_000, 95),
            DynamicContextWindowFailure {
                should_retry: false,
                learned_context_window: Some(32_000),
            }
        );

        let mut state = DynamicContextWindowState::new();
        let _ = state.record_success(40_000, 95);
        assert_eq!(
            state.record_probe_failure("turn-1", 80_000, 95),
            DynamicContextWindowFailure {
                should_retry: true,
                learned_context_window: Some(32_000),
            }
        );
        assert_eq!(
            state.record_probe_failure("turn-1", 80_000, 95),
            DynamicContextWindowFailure {
                should_retry: false,
                learned_context_window: Some(32_000),
            }
        );

        assert_eq!(
            state.record_probe_failure("turn-2", 80_000, 95),
            DynamicContextWindowFailure {
                should_retry: true,
                learned_context_window: Some(32_000),
            }
        );
    }
}
