//! Prefix-key filter: recognizes `Ctrl-b` (0x02) followed by a command.
//!
//! Commands:
//!
//! - `d` / `q` → detach
//! - `h` / `j` / `k` / `l` → focus left / down / up / right
//! - arrow keys (via CSI `ESC [ A/B/C/D`) → focus up / down / right / left
//! - `?` → help overlay (C3; no-op in C1.5)
//!
//! Anything else after `Ctrl-b` cancels the prefix and the raw bytes
//! (Ctrl-b + the trailing bytes) are forwarded as-is, so the user's
//! accidental `Ctrl-b x` still reaches the pty.
//!
//! Across chunks: the filter's state persists so a prefix started at
//! the end of one chunk completes in the next. A chunk that contains
//! a control action ends at that action — bytes after it in the same
//! chunk are dropped, matching tmux's "prefix absorbs the tail" feel.

use crate::tile::FocusDir;

const PREFIX_BYTE: u8 = 0x02; // Ctrl-b
const DETACH_D: u8 = b'd';
const DETACH_Q: u8 = b'q';
const FOCUS_H: u8 = b'h';
const FOCUS_J: u8 = b'j';
const FOCUS_K: u8 = b'k';
const FOCUS_L: u8 = b'l';
const HELP: u8 = b'?';
const ESC: u8 = 0x1b;
const CSI_OPEN: u8 = b'[';
const CSI_UP: u8 = b'A';
const CSI_DOWN: u8 = b'B';
const CSI_RIGHT: u8 = b'C';
const CSI_LEFT: u8 = b'D';

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InputAction {
    /// Forward these bytes: either to the daemon as `SendInput` if the
    /// focused tile is the pty, or to the focused scope's key parser
    /// if it's a scope tile. The filter doesn't know what's focused;
    /// the App routes based on `View.focused`.
    Forward(Vec<u8>),
    /// User pressed the detach sequence.
    Detach,
    /// User pressed `Ctrl-b` + a direction key (h/j/k/l or arrow).
    FocusDirection(FocusDir),
    /// User pressed `Ctrl-b ?` — help overlay (C3).
    Help,
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

    /// Process a chunk of raw input bytes. Control actions (Detach /
    /// FocusDirection / Help) are always final in the chunk; bytes
    /// after them are dropped.
    pub(crate) fn process(&mut self, input: &[u8]) -> Vec<InputAction> {
        let mut actions = Vec::new();
        let mut out = Vec::with_capacity(input.len());

        for &b in input {
            match std::mem::take(&mut self.state) {
                FilterState::Normal => {
                    if b == PREFIX_BYTE {
                        self.state = FilterState::PrefixActive;
                    } else {
                        out.push(b);
                    }
                }
                FilterState::PrefixActive => {
                    let control = match b {
                        DETACH_D | DETACH_Q => Some(InputAction::Detach),
                        FOCUS_H => Some(InputAction::FocusDirection(FocusDir::Left)),
                        FOCUS_J => Some(InputAction::FocusDirection(FocusDir::Down)),
                        FOCUS_K => Some(InputAction::FocusDirection(FocusDir::Up)),
                        FOCUS_L => Some(InputAction::FocusDirection(FocusDir::Right)),
                        HELP => Some(InputAction::Help),
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
                        // Might be the start of an arrow-key CSI.
                        self.state = FilterState::PrefixEsc;
                    } else {
                        // Not a recognized prefix sequence — emit the
                        // buffered Ctrl-b + this byte and keep going.
                        out.push(PREFIX_BYTE);
                        out.push(b);
                        self.state = FilterState::Normal;
                    }
                }
                FilterState::PrefixEsc => {
                    if b == CSI_OPEN {
                        self.state = FilterState::PrefixCsi;
                    } else {
                        // Ctrl-b ESC X where X isn't `[`: forward the
                        // whole thing verbatim and keep going.
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
                    // Unknown CSI final after Ctrl-b — emit the raw
                    // bytes. Parameter bytes inside CSI aren't
                    // supported for focus navigation; anything else
                    // gets forwarded untouched.
                    out.push(PREFIX_BYTE);
                    out.push(ESC);
                    out.push(CSI_OPEN);
                    out.push(b);
                    self.state = FilterState::Normal;
                }
            }
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
    fn ctrl_b_then_q_also_detaches() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02q");
        assert!(matches!(&a[..], [InputAction::Detach]));
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
        // Ctrl-b ESC followed by something that isn't `[` cancels —
        // forward the Ctrl-b ESC and the trailing byte so an accidental
        // mash doesn't vanish into the filter.
        let mut f = InputFilter::new();
        let a = f.process(b"\x02\x1bZ");
        assert!(matches!(&a[..], [InputAction::Forward(v)] if v == b"\x02\x1bZ"));
    }

    #[test]
    fn ctrl_b_csi_unknown_final_forwards_raw() {
        // Ctrl-b ESC [ X where X is not A/B/C/D — forward the raw
        // 4 bytes.
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
        // Split after Ctrl-b.
        let a = f.process(b"\x02");
        assert!(a.is_empty());
        // Split after Ctrl-b ESC.
        let b = f.process(b"\x1b");
        assert!(b.is_empty());
        // Split after Ctrl-b ESC [.
        let c = f.process(b"[");
        assert!(c.is_empty());
        // Final byte in its own chunk.
        let d = f.process(b"A");
        assert!(matches!(
            &d[..],
            [InputAction::FocusDirection(FocusDir::Up)]
        ));
    }

    #[test]
    fn ctrl_b_ctrl_b_emits_both_forwarded() {
        // First Ctrl-b enters prefix; second Ctrl-b after prefix is not
        // a recognized command so the pair emits as raw bytes.
        let mut f = InputFilter::new();
        let a = f.process(b"\x02\x02");
        assert!(matches!(&a[..], [InputAction::Forward(v)] if v == b"\x02\x02"));
    }
}
