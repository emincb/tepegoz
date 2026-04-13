//! Scope-panel renderers. One module per [`crate::app::ScopeKind`].
//!
//! Renderers are pure functions of `(&ScopeState, &mut Frame)` so they can
//! be exercised via `ratatui::backend::TestBackend` in headless render
//! tests. Slice C2 adds the docker render test; Phase 4 adds tests for
//! ports/processes by the same pattern.

pub(crate) mod docker;
