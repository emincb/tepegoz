//! Tile layout + focus navigation for the god-view TUI.
//!
//! The TUI renders a fixed tiled layout per `docs/DECISIONS.md#7`. Each
//! tile has a [`TileId`], a [`TileKind`] (`Pty`, `Scope`, or
//! `Placeholder`), and a `Rect` computed by [`TileLayout::default_for`]
//! from the current terminal size. [`TileLayout::next_focus`] resolves
//! `(TileId, FocusDir)` → adjacent `TileId` for `Ctrl-b h/j/k/l` + arrow
//! keys.
//!
//! Default layout (mockup from `README.md`):
//!
//! ```text
//! ┌──────────────────── PTY ────────────────────────────┐
//! ├─ Docker ────────┬─ Ports ────────┬─ Fleet ──────────┤
//! ├────────────── Claude Code ──────────────────────────┤
//! └─────────────────────────────────────────────────────┘
//! ```
//!
//! Tiny-terminal fallback: if `area.width < MIN_COLS` or
//! `area.height < MIN_ROWS` the god view is unreadable; a single
//! [`TileId::TooSmall`] tile is rendered instead so we don't crash or
//! produce an unreadable screen.

use ratatui::layout::Rect;

use crate::app::ScopeKind;

/// Stable identifier for each tile in the god view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TileId {
    Pty,
    Docker,
    Ports,
    Fleet,
    ClaudeCode,
    /// Single-tile fallback used when the terminal is too small for the
    /// god view.
    TooSmall,
}

/// Content kind for a tile. Determines rendering + input routing.
#[derive(Debug, Clone)]
pub(crate) enum TileKind {
    Pty,
    Scope(ScopeKind),
    /// Not-yet-implemented scope: bordered block, centered label, dim
    /// border. Input is dropped when focused.
    Placeholder {
        label: String,
        eta_phase: u8,
    },
    /// "Terminal too small" notice.
    TooSmall,
}

/// Direction for `Ctrl-b h/j/k/l` (and `Ctrl-b` + arrow key) focus
/// navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

/// One tile: its id, its content kind, and its `Rect` within the
/// terminal.
#[derive(Debug, Clone)]
pub(crate) struct TileDef {
    pub id: TileId,
    pub kind: TileKind,
    pub rect: Rect,
}

/// Fixed tile list + the id of the initially focused tile on fresh
/// construction.
#[derive(Debug, Clone)]
pub(crate) struct TileLayout {
    pub tiles: Vec<TileDef>,
    pub default_focus: TileId,
}

/// Minimum terminal dimensions for the god view. Below this the layout
/// collapses to a single fallback tile.
pub(crate) const MIN_COLS: u16 = 80;
pub(crate) const MIN_ROWS: u16 = 24;

/// Fixed row budget for the Claude Code bottom strip. 3 rows fits a
/// bordered block ("border + content + border") without clipping.
const CLAUDE_ROWS: u16 = 3;

impl TileLayout {
    /// God-view layout for a terminal of the given `area`. Falls back
    /// to a single-tile layout when the terminal is below the minimum
    /// dimensions.
    pub fn default_for(area: Rect) -> Self {
        if area.width < MIN_COLS || area.height < MIN_ROWS {
            return Self::tiny_fallback(area);
        }

        let cc_rows = CLAUDE_ROWS;
        let remaining = area.height - cc_rows;
        // Roughly 60% of the remaining rows go to pty; the rest to the
        // scope row. `.max(6)` keeps vim legible on tight terminals.
        let pty_rows = (remaining * 3 / 5).max(6);
        let scope_rows = remaining - pty_rows;

        // Three equal columns in the scope row; Fleet absorbs the
        // remainder so the row covers the full width.
        let scope_col = area.width / 3;
        let docker_w = scope_col;
        let ports_w = scope_col;
        let fleet_w = area.width - 2 * scope_col;

        let pty_rect = Rect::new(area.x, area.y, area.width, pty_rows);
        let docker_rect = Rect::new(area.x, area.y + pty_rows, docker_w, scope_rows);
        let ports_rect = Rect::new(area.x + docker_w, area.y + pty_rows, ports_w, scope_rows);
        let fleet_rect = Rect::new(
            area.x + docker_w + ports_w,
            area.y + pty_rows,
            fleet_w,
            scope_rows,
        );
        let cc_rect = Rect::new(area.x, area.y + pty_rows + scope_rows, area.width, cc_rows);

        let tiles = vec![
            TileDef {
                id: TileId::Pty,
                kind: TileKind::Pty,
                rect: pty_rect,
            },
            TileDef {
                id: TileId::Docker,
                kind: TileKind::Scope(ScopeKind::Docker),
                rect: docker_rect,
            },
            TileDef {
                id: TileId::Ports,
                kind: TileKind::Scope(ScopeKind::Ports),
                rect: ports_rect,
            },
            TileDef {
                id: TileId::Fleet,
                kind: TileKind::Scope(ScopeKind::Fleet),
                rect: fleet_rect,
            },
            TileDef {
                id: TileId::ClaudeCode,
                kind: TileKind::Placeholder {
                    label: "Claude Code — Phase 9".to_string(),
                    eta_phase: 9,
                },
                rect: cc_rect,
            },
        ];

        Self {
            tiles,
            default_focus: TileId::Pty,
        }
    }

    fn tiny_fallback(area: Rect) -> Self {
        Self {
            tiles: vec![TileDef {
                id: TileId::TooSmall,
                kind: TileKind::TooSmall,
                rect: area,
            }],
            default_focus: TileId::TooSmall,
        }
    }

    pub fn tile(&self, id: TileId) -> Option<&TileDef> {
        self.tiles.iter().find(|t| t.id == id)
    }

    pub fn rect_of(&self, id: TileId) -> Option<Rect> {
        self.tile(id).map(|t| t.rect)
    }

    /// The adjacent tile in `dir` from `from`, or `None` if nothing
    /// qualifies. TooSmall never navigates.
    ///
    /// Primary distance is the gap to the far edge of the candidate in
    /// the nav direction; secondary tiebreak aligns the perpendicular
    /// axis (for up/down: align `tile.x` with `from.x`; for left/right:
    /// align `tile.y` with `from.y`). This makes `j` from the full-width
    /// PTY tile land on Docker (both left-aligned at x=0) rather than
    /// Ports (which is centered under the PTY), which matches the
    /// user's mental model of "down goes to the live scope, not the
    /// placeholder."
    pub fn next_focus(&self, from: TileId, dir: FocusDir) -> Option<TileId> {
        if from == TileId::TooSmall {
            return None;
        }
        let from_rect = self.rect_of(from)?;

        self.tiles
            .iter()
            .filter(|t| t.id != from && t.id != TileId::TooSmall)
            .filter(|t| match dir {
                FocusDir::Up => t.rect.y + t.rect.height <= from_rect.y,
                FocusDir::Down => t.rect.y >= from_rect.y + from_rect.height,
                FocusDir::Left => t.rect.x + t.rect.width <= from_rect.x,
                FocusDir::Right => t.rect.x >= from_rect.x + from_rect.width,
            })
            .min_by_key(|t| {
                let primary = match dir {
                    FocusDir::Up => (from_rect.y as i32) - ((t.rect.y + t.rect.height) as i32),
                    FocusDir::Down => (t.rect.y as i32) - ((from_rect.y + from_rect.height) as i32),
                    FocusDir::Left => (from_rect.x as i32) - ((t.rect.x + t.rect.width) as i32),
                    FocusDir::Right => (t.rect.x as i32) - ((from_rect.x + from_rect.width) as i32),
                };
                let secondary = match dir {
                    FocusDir::Up | FocusDir::Down => (t.rect.x as i32 - from_rect.x as i32).abs(),
                    FocusDir::Left | FocusDir::Right => {
                        (t.rect.y as i32 - from_rect.y as i32).abs()
                    }
                };
                (primary, secondary)
            })
            .map(|t| t.id)
    }

    /// True when this tile should route stdin bytes straight to the
    /// pty as `SendInput`.
    pub fn routes_to_pty(&self, id: TileId) -> bool {
        matches!(self.tile(id).map(|t| &t.kind), Some(TileKind::Pty))
    }

    /// The scope kind a tile dispatches its input to, or `None` if
    /// input should be dropped (placeholders, too-small).
    pub fn routes_to_scope(&self, id: TileId) -> Option<ScopeKind> {
        match self.tile(id).map(|t| &t.kind)? {
            TileKind::Scope(kind) => Some(*kind),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout_at_120x40_has_five_tiles_in_god_view_shape() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = TileLayout::default_for(area);
        assert_eq!(layout.tiles.len(), 5);
        assert_eq!(layout.default_focus, TileId::Pty);

        let pty = layout.rect_of(TileId::Pty).unwrap();
        let docker = layout.rect_of(TileId::Docker).unwrap();
        let ports = layout.rect_of(TileId::Ports).unwrap();
        let fleet = layout.rect_of(TileId::Fleet).unwrap();
        let cc = layout.rect_of(TileId::ClaudeCode).unwrap();

        // PTY occupies the top full-width strip.
        assert_eq!(pty.x, 0);
        assert_eq!(pty.y, 0);
        assert_eq!(pty.width, 120);

        // Scope row starts directly below PTY and covers full width.
        assert_eq!(docker.y, pty.y + pty.height);
        assert_eq!(ports.y, docker.y);
        assert_eq!(fleet.y, docker.y);
        assert_eq!(docker.x, 0);
        assert_eq!(ports.x, docker.x + docker.width);
        assert_eq!(fleet.x, ports.x + ports.width);
        assert_eq!(docker.width + ports.width + fleet.width, 120);

        // Claude Code strip full-width at the bottom.
        assert_eq!(cc.x, 0);
        assert_eq!(cc.width, 120);
        assert_eq!(cc.height, CLAUDE_ROWS);
        assert_eq!(cc.y + cc.height, 40);
    }

    #[test]
    fn tiny_terminal_falls_back_to_single_too_small_tile() {
        let area = Rect::new(0, 0, 60, 20);
        let layout = TileLayout::default_for(area);
        assert_eq!(layout.tiles.len(), 1);
        assert_eq!(layout.default_focus, TileId::TooSmall);
        assert!(matches!(layout.tiles[0].kind, TileKind::TooSmall));
        assert_eq!(layout.tiles[0].rect, area);
    }

    #[test]
    fn just_at_minimum_size_still_renders_full_god_view() {
        let area = Rect::new(0, 0, MIN_COLS, MIN_ROWS);
        let layout = TileLayout::default_for(area);
        assert_eq!(layout.tiles.len(), 5);
    }

    #[test]
    fn focus_down_from_pty_lands_on_docker() {
        let layout = TileLayout::default_for(Rect::new(0, 0, 120, 40));
        let next = layout.next_focus(TileId::Pty, FocusDir::Down).unwrap();
        assert_eq!(
            next,
            TileId::Docker,
            "down from the full-width PTY biases toward Docker (left-aligned) \
             rather than Ports (centered) — matches 'down goes to the live scope'"
        );
    }

    #[test]
    fn focus_up_from_docker_returns_pty() {
        let layout = TileLayout::default_for(Rect::new(0, 0, 120, 40));
        assert_eq!(
            layout.next_focus(TileId::Docker, FocusDir::Up),
            Some(TileId::Pty)
        );
    }

    #[test]
    fn focus_right_from_docker_returns_ports() {
        let layout = TileLayout::default_for(Rect::new(0, 0, 120, 40));
        assert_eq!(
            layout.next_focus(TileId::Docker, FocusDir::Right),
            Some(TileId::Ports)
        );
    }

    #[test]
    fn focus_right_from_ports_returns_fleet() {
        let layout = TileLayout::default_for(Rect::new(0, 0, 120, 40));
        assert_eq!(
            layout.next_focus(TileId::Ports, FocusDir::Right),
            Some(TileId::Fleet)
        );
    }

    #[test]
    fn focus_left_from_fleet_returns_ports() {
        let layout = TileLayout::default_for(Rect::new(0, 0, 120, 40));
        assert_eq!(
            layout.next_focus(TileId::Fleet, FocusDir::Left),
            Some(TileId::Ports)
        );
    }

    #[test]
    fn focus_down_from_docker_returns_claude_code() {
        let layout = TileLayout::default_for(Rect::new(0, 0, 120, 40));
        assert_eq!(
            layout.next_focus(TileId::Docker, FocusDir::Down),
            Some(TileId::ClaudeCode)
        );
    }

    #[test]
    fn focus_up_from_claude_code_returns_docker() {
        let layout = TileLayout::default_for(Rect::new(0, 0, 120, 40));
        assert_eq!(
            layout.next_focus(TileId::ClaudeCode, FocusDir::Up),
            Some(TileId::Docker),
            "up from full-width Claude Code picks leftmost scope tile"
        );
    }

    #[test]
    fn focus_up_from_pty_returns_none() {
        let layout = TileLayout::default_for(Rect::new(0, 0, 120, 40));
        assert_eq!(layout.next_focus(TileId::Pty, FocusDir::Up), None);
    }

    #[test]
    fn focus_left_from_pty_returns_none() {
        // PTY is full-width; nothing to the left.
        let layout = TileLayout::default_for(Rect::new(0, 0, 120, 40));
        assert_eq!(layout.next_focus(TileId::Pty, FocusDir::Left), None);
        assert_eq!(layout.next_focus(TileId::Pty, FocusDir::Right), None);
    }

    #[test]
    fn focus_down_from_claude_code_returns_none() {
        let layout = TileLayout::default_for(Rect::new(0, 0, 120, 40));
        assert_eq!(layout.next_focus(TileId::ClaudeCode, FocusDir::Down), None);
    }

    #[test]
    fn too_small_tile_never_navigates() {
        let layout = TileLayout::default_for(Rect::new(0, 0, 60, 20));
        for dir in [
            FocusDir::Up,
            FocusDir::Down,
            FocusDir::Left,
            FocusDir::Right,
        ] {
            assert_eq!(layout.next_focus(TileId::TooSmall, dir), None);
        }
    }

    #[test]
    fn routes_to_pty_only_for_pty_tile() {
        let layout = TileLayout::default_for(Rect::new(0, 0, 120, 40));
        assert!(layout.routes_to_pty(TileId::Pty));
        assert!(!layout.routes_to_pty(TileId::Docker));
        assert!(!layout.routes_to_pty(TileId::Ports));
    }

    #[test]
    fn routes_to_scope_returns_scope_kind_only_for_scope_tiles() {
        let layout = TileLayout::default_for(Rect::new(0, 0, 120, 40));
        assert_eq!(
            layout.routes_to_scope(TileId::Docker),
            Some(ScopeKind::Docker)
        );
        assert_eq!(
            layout.routes_to_scope(TileId::Ports),
            Some(ScopeKind::Ports),
            "Phase 4 Slice 4c replaced the Ports placeholder with a real scope"
        );
        assert_eq!(
            layout.routes_to_scope(TileId::Fleet),
            Some(ScopeKind::Fleet),
            "Phase 5 Slice 5b replaced the Fleet placeholder with a real scope"
        );
        assert_eq!(layout.routes_to_scope(TileId::Pty), None);
    }
}
