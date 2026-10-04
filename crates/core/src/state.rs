use crate::log::Scope;

const SLOG: Scope = Scope::new("state");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VisibilityState {
    Visible,
    /// Auto-hidden via hideWhenNoEveFocus; can be auto-shown again.
    HiddenAutomatic,
    /// Persists until the user manually toggles it again.
    HiddenManual,
}

impl VisibilityState {
    pub fn can_transition_to(self, next: VisibilityState) -> bool {
        match self {
            Self::Visible => true,
            Self::HiddenAutomatic => matches!(next, Self::Visible | Self::HiddenManual),
            Self::HiddenManual => matches!(next, Self::Visible | Self::HiddenAutomatic),
        }
    }

    pub fn is_visible(self) -> bool {
        self == Self::Visible
    }
}

/// Used purely as a style-lookup key (config's get_state_config) - never persisted per-thumbnail; see ThumbnailWindow::effective_render_state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThumbnailState {
    Inactive,
    Active,
    Alert,
    Minimized,
    Dragging,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidVisibilityTransition;

impl std::fmt::Display for InvalidVisibilityTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid visibility transition")
    }
}

impl std::error::Error for InvalidVisibilityTransition {}

pub fn transition_visibility(
    current: VisibilityState,
    next: VisibilityState,
    context_name: &str,
) -> Result<VisibilityState, InvalidVisibilityTransition> {
    if !current.can_transition_to(next) {
        SLOG.warn(format_args!("Invalid visibility transition for '{context_name}': {current:?} -> {next:?}"));
        return Err(InvalidVisibilityTransition);
    }

    if current != next {
        SLOG.debug(format_args!("Visibility transition for '{context_name}': {current:?} -> {next:?}"));
    }

    Ok(next)
}

/// Like transition_visibility, but returns `current` instead of erroring on an invalid transition.
pub fn try_transition_visibility(current: VisibilityState, next: VisibilityState, context_name: &str) -> VisibilityState {
    transition_visibility(current, next, context_name).unwrap_or(current)
}

#[cfg(test)]
mod tests {
    use super::VisibilityState::*;
    use super::*;

    #[test]
    fn visible_can_go_anywhere() {
        for next in [Visible, HiddenAutomatic, HiddenManual] {
            assert!(Visible.can_transition_to(next));
        }
    }

    #[test]
    fn hidden_states_cannot_self_transition() {
        assert!(!HiddenAutomatic.can_transition_to(HiddenAutomatic));
        assert!(!HiddenManual.can_transition_to(HiddenManual));
        assert_eq!(try_transition_visibility(HiddenManual, HiddenManual, "t"), HiddenManual);
        assert!(transition_visibility(HiddenManual, HiddenManual, "t").is_err());
    }
}
