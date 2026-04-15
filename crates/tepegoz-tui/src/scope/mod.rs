//! Scope-panel renderers. One module per [`crate::app::ScopeKind`], plus
//! a [`placeholder`] module for not-yet-implemented tiles in the
//! god-view layout.
//!
//! Renderers all take `(state, &mut Frame, Rect, focused: bool,
//! hovered: bool)` per the scope rendering contract in
//! `docs/ARCHITECTURE.md` §9 (extended by Slice 6.0 to carry the
//! hover state), so they can be exercised via
//! `ratatui::backend::TestBackend` in headless render tests AND drawn
//! into a tile sub-`Rect` at runtime.

pub(crate) mod docker;
pub(crate) mod fleet;
pub(crate) mod placeholder;
pub(crate) mod ports;

use ratatui::style::{Color, Modifier};

/// Slice 6.0: shared border-style picker for every scope tile. The
/// three visual states are:
///
/// - **Focused** (bright cyan, no dim modifier) — this tile owns
///   the keystroke stream.
/// - **Hovered & unfocused** (cyan + dim) — the pointer is here
///   but focus lives elsewhere; clicking will focus this tile.
/// - **Idle** (dark gray + dim) — pre-6.0 unfocused style; nothing
///   special about this tile right now.
///
/// Focus wins over hover if both are true (the renderer dispatch
/// clears `hovered` whenever `focused` is set, so this is just
/// belt-and-suspenders).
pub(crate) fn border_style(focused: bool, hovered: bool) -> (Color, Modifier) {
    if focused {
        (Color::Cyan, Modifier::empty())
    } else if hovered {
        (Color::Cyan, Modifier::DIM)
    } else {
        (Color::DarkGray, Modifier::DIM)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focused_tile_uses_bright_cyan_without_dim() {
        let (color, modifier) = border_style(true, false);
        assert_eq!(color, Color::Cyan);
        assert_eq!(modifier, Modifier::empty());
    }

    #[test]
    fn hovered_unfocused_tile_uses_dim_cyan() {
        let (color, modifier) = border_style(false, true);
        assert_eq!(color, Color::Cyan);
        assert_eq!(modifier, Modifier::DIM);
    }

    #[test]
    fn idle_unfocused_tile_uses_dim_dark_gray() {
        let (color, modifier) = border_style(false, false);
        assert_eq!(color, Color::DarkGray);
        assert_eq!(modifier, Modifier::DIM);
    }

    #[test]
    fn focus_wins_over_hover_when_both_set() {
        // The render dispatch clears `hovered` when `focused` is set,
        // but belt-and-suspenders: even if both fire the focused
        // style wins.
        let (color, modifier) = border_style(true, true);
        assert_eq!(color, Color::Cyan);
        assert_eq!(modifier, Modifier::empty());
    }
}
