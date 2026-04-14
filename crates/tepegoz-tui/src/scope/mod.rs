//! Scope-panel renderers. One module per [`crate::app::ScopeKind`], plus
//! a [`placeholder`] module for not-yet-implemented tiles in the
//! god-view layout.
//!
//! Renderers all take `(state, &mut Frame, Rect, focused: bool)` per
//! the scope rendering contract in `docs/ARCHITECTURE.md` §9, so they
//! can be exercised via `ratatui::backend::TestBackend` in headless
//! render tests AND drawn into a tile sub-`Rect` at runtime.

pub(crate) mod docker;
pub(crate) mod placeholder;
pub(crate) mod ports;
