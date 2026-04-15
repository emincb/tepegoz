//! Stdin byte filter — recognizes the minimum documented keybind surface
//! and lets everything else flow through to the focused pty / scope tile.
//!
//! Slice 6.0 (2026-04-15) simplified the surface to five documented
//! bindings: `Tab` / `Shift-Tab` (tile focus cycle), arrow keys / `j`
//! / `k` (row nav inside scope tiles — handled downstream, not here),
//! `Enter` (primary action on selected row — handled downstream),
//! `Esc` (cancel / back — handled downstream), and `Ctrl-b d`
//! (detach). The `Ctrl-b h/j/k/l` spatial-focus bindings and `Ctrl-b &`
//! close-active-pane binding survive as undocumented power-user
//! aliases. `Ctrl-b ?` opens the help overlay. `Ctrl-b 1..9`,
//! `Ctrl-b 0`, `Ctrl-b n`, `Ctrl-b p`, `Ctrl-b w`, `Ctrl-b q`, and
//! `Ctrl-b Enter` are gone — obsoleted by the clickable tab strip
//! (Phase 5 Slice 5d-ii) and the unified `Tab` / `Enter` keys.
//!
//! Bare-escape subtlety: an isolated `ESC` (0x1b) press in vim or a
//! shell must pass through to the pty right away. The filter
//! lookahead-gates entry into bare-CSI parsing: if `ESC` is the last
//! byte of a chunk (no `[` following inside the same chunk), it
//! flushes as-is instead of buffering into `BareCsi`. The edge case
//! where a terminal splits `\x1b[Z` (Shift-Tab) across two stdin
//! reads is documented as a v1 limitation — modern terminals emit
//! such sequences atomically.
//!
//! Chunk-spanning: the `Ctrl-b` prefix state is preserved across
//! reads, same as the pre-6.0 filter. The bare-CSI state is also
//! preserved once it's legitimately entered (both `ESC` and `[` seen
//! in the same chunk).

use crate::tile::FocusDir;

const PREFIX_BYTE: u8 = 0x02; // Ctrl-b
const DETACH_D: u8 = b'd';
const FOCUS_H: u8 = b'h';
const FOCUS_J: u8 = b'j';
const FOCUS_K: u8 = b'k';
const FOCUS_L: u8 = b'l';
const HELP: u8 = b'?';
const PANE_CLOSE: u8 = b'&';
const TAB: u8 = 0x09;
const ESC: u8 = 0x1b;
const CSI_OPEN: u8 = b'[';
const CSI_UP: u8 = b'A';
const CSI_DOWN: u8 = b'B';
const CSI_RIGHT: u8 = b'C';
const CSI_LEFT: u8 = b'D';
const CSI_SHIFT_TAB: u8 = b'Z';

/// ECMA-48 CSI final byte range (0x40..=0x7e).
fn is_csi_final(b: u8) -> bool {
    (0x40..=0x7e).contains(&b)
}

/// ECMA-48 CSI parameter or intermediate byte range (0x20..=0x3f).
fn is_csi_param_or_intermediate(b: u8) -> bool {
    (0x20..=0x3f).contains(&b)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InputAction {
    /// Forward these bytes to the focused tile (pty → `SendInput`;
    /// scope tile → scope key parser; placeholder → drop). The filter
    /// doesn't know what's focused; the App routes based on
    /// `View.focused`.
    Forward(Vec<u8>),
    /// User pressed `Ctrl-b d` — detach.
    Detach,
    /// Directional tile focus move — `Ctrl-b h/j/k/l` or their arrow
    /// equivalents `Ctrl-b ESC [ A/B/C/D`. Kept as an undocumented
    /// power-user alias after Slice 6.0 (the documented surface is
    /// `Tab` / `Shift-Tab`).
    FocusDirection(FocusDir),
    /// Tile focus cycle forward (`Tab`).
    FocusNext,
    /// Tile focus cycle backward (`Shift-Tab` → `ESC [ Z`).
    FocusPrev,
    /// User pressed `Ctrl-b ?` — toggle the help overlay.
    Help,
    /// User pressed `Ctrl-b &` — close the active pane. App opens a
    /// fresh local root if this would empty the stack. Undocumented
    /// after Slice 6.0; the documented path is a click on the tab
    /// strip's close affordance.
    PaneClose,
}

#[derive(Debug, Default)]
enum FilterState {
    /// Normal byte accumulation.
    #[default]
    Normal,
    /// Saw `Ctrl-b`; next byte disambiguates.
    PrefixActive,
    /// Saw `Ctrl-b ESC`; waiting for `[` to enter CSI mode.
    PrefixEsc,
    /// Saw `Ctrl-b ESC [`; waiting for a final byte (arrow final).
    PrefixCsi,
    /// Saw bare `ESC [` (lookahead-gated: both bytes in same chunk).
    /// Distinguish `Z` (Shift-Tab → FocusPrev) from other CSI finals
    /// which flush raw to preserve arrow keys / home / end / etc.
    BareCsi,
    /// Saw bare `ESC [` followed by param/intermediate bytes; still
    /// waiting for the CSI final. Buffers params for verbatim flush
    /// on completion.
    BareCsiCollect { buf: Vec<u8> },
}

pub(crate) struct InputFilter {
    state: FilterState,
}

impl InputFilter {
    pub(crate) fn new() -> Self {
        Self {
            state: FilterState::Normal,
        }
    }

    /// Process a chunk of raw input bytes. Control actions are always
    /// final in the chunk; bytes after them are dropped (tmux-like
    /// "prefix absorbs the tail" feel).
    pub(crate) fn process(&mut self, input: &[u8]) -> Vec<InputAction> {
        let mut actions = Vec::new();
        let mut out = Vec::with_capacity(input.len());
        let mut i = 0;

        while i < input.len() {
            let b = input[i];
            match std::mem::take(&mut self.state) {
                FilterState::Normal => {
                    if b == TAB {
                        if !out.is_empty() {
                            actions.push(InputAction::Forward(std::mem::take(&mut out)));
                        }
                        actions.push(InputAction::FocusNext);
                        return actions;
                    } else if b == PREFIX_BYTE {
                        self.state = FilterState::PrefixActive;
                    } else if b == ESC && i + 1 < input.len() && input[i + 1] == CSI_OPEN {
                        // Both ESC and `[` are present in this chunk
                        // — safe to enter bare-CSI parsing without
                        // stranding a bare-Esc keystroke. Consume
                        // both bytes.
                        self.state = FilterState::BareCsi;
                        i += 1;
                    } else {
                        out.push(b);
                    }
                }
                FilterState::PrefixActive => {
                    let control = match b {
                        DETACH_D => Some(InputAction::Detach),
                        FOCUS_H => Some(InputAction::FocusDirection(FocusDir::Left)),
                        FOCUS_J => Some(InputAction::FocusDirection(FocusDir::Down)),
                        FOCUS_K => Some(InputAction::FocusDirection(FocusDir::Up)),
                        FOCUS_L => Some(InputAction::FocusDirection(FocusDir::Right)),
                        HELP => Some(InputAction::Help),
                        PANE_CLOSE => Some(InputAction::PaneClose),
                        _ => None,
                    };
                    if let Some(action) = control {
                        if !out.is_empty() {
                            actions.push(InputAction::Forward(std::mem::take(&mut out)));
                        }
                        actions.push(action);
                        self.state = FilterState::Normal;
                        return actions;
                    }
                    if b == ESC {
                        self.state = FilterState::PrefixEsc;
                    } else {
                        out.push(PREFIX_BYTE);
                        out.push(b);
                        self.state = FilterState::Normal;
                    }
                }
                FilterState::PrefixEsc => {
                    if b == CSI_OPEN {
                        self.state = FilterState::PrefixCsi;
                    } else {
                        out.push(PREFIX_BYTE);
                        out.push(ESC);
                        out.push(b);
                        self.state = FilterState::Normal;
                    }
                }
                FilterState::PrefixCsi => {
                    let dir = match b {
                        CSI_UP => Some(FocusDir::Up),
                        CSI_DOWN => Some(FocusDir::Down),
                        CSI_RIGHT => Some(FocusDir::Right),
                        CSI_LEFT => Some(FocusDir::Left),
                        _ => None,
                    };
                    if let Some(dir) = dir {
                        if !out.is_empty() {
                            actions.push(InputAction::Forward(std::mem::take(&mut out)));
                        }
                        actions.push(InputAction::FocusDirection(dir));
                        self.state = FilterState::Normal;
                        return actions;
                    }
                    out.push(PREFIX_BYTE);
                    out.push(ESC);
                    out.push(CSI_OPEN);
                    out.push(b);
                    self.state = FilterState::Normal;
                }
                FilterState::BareCsi => {
                    if b == CSI_SHIFT_TAB {
                        if !out.is_empty() {
                            actions.push(InputAction::Forward(std::mem::take(&mut out)));
                        }
                        actions.push(InputAction::FocusPrev);
                        self.state = FilterState::Normal;
                        return actions;
                    } else if is_csi_final(b) {
                        out.push(ESC);
                        out.push(CSI_OPEN);
                        out.push(b);
                        self.state = FilterState::Normal;
                    } else if is_csi_param_or_intermediate(b) {
                        self.state = FilterState::BareCsiCollect { buf: vec![b] };
                    } else {
                        // Malformed CSI — flush the prefix plus this
                        // byte. Consistent with the PrefixCsi fallback.
                        out.push(ESC);
                        out.push(CSI_OPEN);
                        out.push(b);
                        self.state = FilterState::Normal;
                    }
                }
                FilterState::BareCsiCollect { mut buf } => {
                    if is_csi_final(b) {
                        out.push(ESC);
                        out.push(CSI_OPEN);
                        out.extend_from_slice(&buf);
                        out.push(b);
                        self.state = FilterState::Normal;
                    } else if is_csi_param_or_intermediate(b) {
                        buf.push(b);
                        self.state = FilterState::BareCsiCollect { buf };
                    } else {
                        out.push(ESC);
                        out.push(CSI_OPEN);
                        out.extend_from_slice(&buf);
                        out.push(b);
                        self.state = FilterState::Normal;
                    }
                }
            }
            i += 1;
        }

        if !out.is_empty() {
            actions.push(InputAction::Forward(out));
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pass_through_plain_bytes() {
        let mut f = InputFilter::new();
        let a = f.process(b"ls -la\n");
        assert!(matches!(&a[..], [InputAction::Forward(v)] if v == b"ls -la\n"));
    }

    #[test]
    fn ctrl_b_then_d_detaches() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02d");
        assert!(matches!(&a[..], [InputAction::Detach]));
    }

    #[test]
    fn ctrl_b_then_q_is_not_detach_after_6_0() {
        // Slice 6.0 removed `Ctrl-b q` — the unrecognized prefix
        // sequence flushes `\x02q` as raw bytes instead of acting.
        let mut f = InputFilter::new();
        let a = f.process(b"\x02q");
        assert!(matches!(&a[..], [InputAction::Forward(v)] if v == b"\x02q"));
    }

    #[test]
    fn ctrl_b_then_h_focuses_left() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02h");
        assert!(matches!(
            &a[..],
            [InputAction::FocusDirection(FocusDir::Left)]
        ));
    }

    #[test]
    fn ctrl_b_then_j_focuses_down() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02j");
        assert!(matches!(
            &a[..],
            [InputAction::FocusDirection(FocusDir::Down)]
        ));
    }

    #[test]
    fn ctrl_b_then_k_focuses_up() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02k");
        assert!(matches!(
            &a[..],
            [InputAction::FocusDirection(FocusDir::Up)]
        ));
    }

    #[test]
    fn ctrl_b_then_l_focuses_right() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02l");
        assert!(matches!(
            &a[..],
            [InputAction::FocusDirection(FocusDir::Right)]
        ));
    }

    #[test]
    fn ctrl_b_then_up_arrow_focuses_up() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02\x1b[A");
        assert!(matches!(
            &a[..],
            [InputAction::FocusDirection(FocusDir::Up)]
        ));
    }

    #[test]
    fn ctrl_b_then_down_arrow_focuses_down() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02\x1b[B");
        assert!(matches!(
            &a[..],
            [InputAction::FocusDirection(FocusDir::Down)]
        ));
    }

    #[test]
    fn ctrl_b_then_left_arrow_focuses_left() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02\x1b[D");
        assert!(matches!(
            &a[..],
            [InputAction::FocusDirection(FocusDir::Left)]
        ));
    }

    #[test]
    fn ctrl_b_then_right_arrow_focuses_right() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02\x1b[C");
        assert!(matches!(
            &a[..],
            [InputAction::FocusDirection(FocusDir::Right)]
        ));
    }

    #[test]
    fn ctrl_b_then_question_opens_help() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02?");
        assert!(matches!(&a[..], [InputAction::Help]));
    }

    #[test]
    fn ctrl_b_then_other_forwards_both() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02x");
        assert!(matches!(&a[..], [InputAction::Forward(v)] if v == b"\x02x"));
    }

    #[test]
    fn ctrl_b_then_esc_then_unknown_forwards_raw() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02\x1bZ");
        assert!(matches!(&a[..], [InputAction::Forward(v)] if v == b"\x02\x1bZ"));
    }

    #[test]
    fn ctrl_b_csi_unknown_final_forwards_raw() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02\x1b[Z");
        assert!(matches!(&a[..], [InputAction::Forward(v)] if v == b"\x02\x1b[Z"));
    }

    #[test]
    fn detach_splits_stream() {
        let mut f = InputFilter::new();
        let a = f.process(b"ls\n\x02d");
        assert_eq!(a.len(), 2);
        assert!(matches!(&a[0], InputAction::Forward(v) if v == b"ls\n"));
        assert!(matches!(&a[1], InputAction::Detach));
    }

    #[test]
    fn focus_direction_splits_stream() {
        let mut f = InputFilter::new();
        let a = f.process(b"ls\n\x02j");
        assert_eq!(a.len(), 2);
        assert!(matches!(&a[0], InputAction::Forward(v) if v == b"ls\n"));
        assert!(matches!(&a[1], InputAction::FocusDirection(FocusDir::Down)));
    }

    #[test]
    fn ctrl_b_split_across_chunks() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02");
        assert!(a.is_empty());
        let b = f.process(b"d");
        assert!(matches!(&b[..], [InputAction::Detach]));
    }

    #[test]
    fn ctrl_b_j_split_across_chunks() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02");
        assert!(a.is_empty());
        let b = f.process(b"j");
        assert!(matches!(
            &b[..],
            [InputAction::FocusDirection(FocusDir::Down)]
        ));
    }

    #[test]
    fn ctrl_b_arrow_split_across_chunks() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02");
        assert!(a.is_empty());
        let b = f.process(b"\x1b");
        assert!(b.is_empty());
        let c = f.process(b"[");
        assert!(c.is_empty());
        let d = f.process(b"A");
        assert!(matches!(
            &d[..],
            [InputAction::FocusDirection(FocusDir::Up)]
        ));
    }

    #[test]
    fn ctrl_b_ctrl_b_emits_both_forwarded() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02\x02");
        assert!(matches!(&a[..], [InputAction::Forward(v)] if v == b"\x02\x02"));
    }

    #[test]
    fn ctrl_b_then_ampersand_closes_pane() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02&");
        assert!(matches!(&a[..], [InputAction::PaneClose]));
    }

    // --- Slice 6.0: removed bindings flush as raw bytes -------------

    #[test]
    fn ctrl_b_then_n_is_not_pane_next_after_6_0() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02n");
        assert!(matches!(&a[..], [InputAction::Forward(v)] if v == b"\x02n"));
    }

    #[test]
    fn ctrl_b_then_p_is_not_pane_prev_after_6_0() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02p");
        assert!(matches!(&a[..], [InputAction::Forward(v)] if v == b"\x02p"));
    }

    #[test]
    fn ctrl_b_then_digit_is_not_pane_select_after_6_0() {
        for d in 0..=9u8 {
            let mut f = InputFilter::new();
            let buf = [PREFIX_BYTE, b'0' + d];
            let a = f.process(&buf);
            assert!(
                matches!(&a[..], [InputAction::Forward(v)] if v == &buf),
                "digit {d} must flush as raw \\x02<digit> bytes after 6.0"
            );
        }
    }

    #[test]
    fn ctrl_b_then_w_is_not_pane_list_after_6_0() {
        // Pre-6.0 this was silently consumed for the deferred overlay;
        // post-6.0 there is no pane-list overlay plan and the bytes
        // flush raw alongside the other deleted bindings.
        let mut f = InputFilter::new();
        let a = f.process(b"\x02w");
        assert!(matches!(&a[..], [InputAction::Forward(v)] if v == b"\x02w"));
    }

    #[test]
    fn ctrl_b_then_enter_forwards_raw_after_6_0() {
        // Pre-6.0 this opened a remote pane on Fleet. Post-6.0 plain
        // `Enter` (no Ctrl-b prefix) handles that via the scope key
        // parser; the legacy Ctrl-b Enter sequence flushes raw.
        let mut f = InputFilter::new();
        let a = f.process(b"\x02\r");
        assert!(matches!(&a[..], [InputAction::Forward(v)] if v == b"\x02\r"));
    }

    // --- Slice 6.0: new Tab / Shift-Tab intercepts ------------------

    #[test]
    fn bare_tab_emits_focus_next() {
        let mut f = InputFilter::new();
        let a = f.process(b"\t");
        assert!(matches!(&a[..], [InputAction::FocusNext]));
    }

    #[test]
    fn bare_shift_tab_emits_focus_prev() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x1b[Z");
        assert!(matches!(&a[..], [InputAction::FocusPrev]));
    }

    #[test]
    fn tab_splits_stream_and_drops_tail() {
        let mut f = InputFilter::new();
        let a = f.process(b"abc\txyz");
        assert_eq!(a.len(), 2);
        assert!(matches!(&a[0], InputAction::Forward(v) if v == b"abc"));
        assert!(matches!(&a[1], InputAction::FocusNext));
    }

    #[test]
    fn bare_arrow_keys_pass_through_to_out() {
        // Arrow keys must still flow to focused scope/pty — the bare
        // CSI parser flushes them raw.
        let mut f = InputFilter::new();
        let a = f.process(b"\x1b[A");
        assert!(matches!(&a[..], [InputAction::Forward(v)] if v == b"\x1b[A"));
    }

    #[test]
    fn bare_page_down_passes_through_to_out() {
        // PageDown is `\x1b[6~` — a CSI with a parameter byte. The
        // bare-CSI collect state must flush the full sequence.
        let mut f = InputFilter::new();
        let a = f.process(b"\x1b[6~");
        assert!(matches!(&a[..], [InputAction::Forward(v)] if v == b"\x1b[6~"));
    }

    #[test]
    fn bare_esc_alone_passes_through_without_waiting() {
        // A chunk that ends on bare ESC must flush the ESC to out
        // immediately (the lookahead fails because `[` isn't in the
        // same chunk) so vim / shell Esc keystrokes register without
        // waiting for the next key. Documented v1 trade-off: a
        // genuinely cross-chunk `\x1b[Z` (Shift-Tab) will miss the
        // intercept.
        let mut f = InputFilter::new();
        let a = f.process(b"\x1b");
        assert!(matches!(&a[..], [InputAction::Forward(v)] if v == b"\x1b"));
    }

    #[test]
    fn bare_shift_tab_split_across_chunks_after_lookahead_commit() {
        // If the first chunk contains both `\x1b` and `[`, the
        // lookahead commits the filter into `BareCsi`. The `Z` in
        // the next chunk then completes the Shift-Tab intercept.
        let mut f = InputFilter::new();
        let a = f.process(b"\x1b[");
        assert!(a.is_empty());
        let b = f.process(b"Z");
        assert!(matches!(&b[..], [InputAction::FocusPrev]));
    }
}
