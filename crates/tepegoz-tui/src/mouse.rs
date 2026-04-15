//! SGR-mode mouse event parser.
//!
//! Crossterm's `EnableMouseCapture` puts the terminal in SGR mode
//! (DEC `?1006h`) plus any-motion tracking (`?1003h`), so mouse
//! events arrive on stdin as escape sequences rather than a
//! separate channel. Format:
//!
//! ```text
//! ESC [ < Cb ; Cx ; Cy M      (button press)
//! ESC [ < Cb ; Cx ; Cy m      (button release)
//! ```
//!
//! where `Cb` is a bitfield (low 2 bits = button index; bit 5 = motion;
//! bit 6 = wheel; higher bits for modifiers), and `Cx`/`Cy` are
//! 1-indexed column/row (SGR fixed the old 223-cap that X10 had).
//!
//! Slice 6.0 cares about three events:
//! - left-button press → `AppEvent::MouseClick`
//! - any-motion (button held or not) → `AppEvent::MouseHover`
//! - everything else (right/middle, release, wheel, modifiers) is
//!   dropped silently — the clickable surface is left-click only.
//!
//! The parser runs on the raw stdin byte stream before `InputFilter`
//! sees it, stripping matched SGR sequences and passing the
//! remaining bytes through. Non-SGR escape sequences (Shift-Tab,
//! arrow keys, F-keys, etc.) are flushed back verbatim so
//! `InputFilter`'s CSI handling still sees them.
//!
//! Cross-chunk safety: sequence state is preserved across calls so
//! a terminal that happens to split `\x1b[<0;10;5M` over two reads
//! still resolves. The bare-`\x1b` delay-until-next-key caveat
//! documented in `input.rs` applies here too — a user's plain Esc
//! keystroke doesn't reach the pty until the next byte arrives (or
//! a non-`[` follow-up forces a flush). Real terminals emit mouse
//! sequences atomically so the state machine's cross-chunk path is
//! mostly an insurance policy.

use crate::app::AppEvent;

const ESC: u8 = 0x1b;
const CSI_OPEN: u8 = b'[';
const SGR_MARKER: u8 = b'<';
const SGR_PRESS: u8 = b'M';
const SGR_RELEASE: u8 = b'm';

/// SGR button code bits.
const BUTTON_MASK: u32 = 0b11;
const MOTION_BIT: u32 = 0b100000; // 32
const WHEEL_BIT: u32 = 0b1000000; // 64

#[derive(Debug, Default)]
enum MouseState {
    #[default]
    Idle,
    /// Saw a bare `ESC` — could be the start of a mouse sequence,
    /// a keyboard CSI (arrow / Shift-Tab), or a plain Esc keystroke.
    Esc,
    /// Saw `ESC [` — still ambiguous until the next byte.
    Csi,
    /// Saw `ESC [ <` — committed to SGR mouse, collecting params
    /// until the `M` / `m` final.
    Params { buf: Vec<u8> },
}

pub(crate) struct MouseParser {
    state: MouseState,
}

impl MouseParser {
    pub(crate) fn new() -> Self {
        Self {
            state: MouseState::Idle,
        }
    }

    /// Consume a chunk of stdin bytes. Returns `(remaining, events)`
    /// where `remaining` contains the bytes that were not part of an
    /// SGR mouse sequence (to be fed onward to `InputFilter`) and
    /// `events` is the list of extracted `AppEvent::MouseClick` /
    /// `MouseHover` events in order.
    pub(crate) fn parse(&mut self, input: &[u8]) -> (Vec<u8>, Vec<AppEvent>) {
        let mut out = Vec::with_capacity(input.len());
        let mut events = Vec::new();

        for &b in input {
            match std::mem::take(&mut self.state) {
                MouseState::Idle => {
                    if b == ESC {
                        self.state = MouseState::Esc;
                    } else {
                        out.push(b);
                    }
                }
                MouseState::Esc => {
                    if b == CSI_OPEN {
                        self.state = MouseState::Csi;
                    } else {
                        out.push(ESC);
                        out.push(b);
                        self.state = MouseState::Idle;
                    }
                }
                MouseState::Csi => {
                    if b == SGR_MARKER {
                        self.state = MouseState::Params { buf: Vec::new() };
                    } else {
                        // Not SGR — flush `ESC [ b` so InputFilter
                        // sees it (Shift-Tab, arrow key, etc).
                        out.push(ESC);
                        out.push(CSI_OPEN);
                        out.push(b);
                        self.state = MouseState::Idle;
                    }
                }
                MouseState::Params { mut buf } => {
                    if b == SGR_PRESS || b == SGR_RELEASE {
                        if let Some(event) = parse_sgr_params(&buf, b == SGR_PRESS) {
                            events.push(event);
                        }
                        self.state = MouseState::Idle;
                    } else {
                        // Accumulate param bytes. SGR params are
                        // digits and `;`; anything else is malformed,
                        // but we collect silently until a terminator
                        // so the entire malformed sequence gets
                        // dropped rather than leaking bytes that
                        // would confuse InputFilter.
                        buf.push(b);
                        self.state = MouseState::Params { buf };
                    }
                }
            }
        }

        (out, events)
    }
}

/// Parse `Cb;Cx;Cy` (the bytes between `<` and the `M`/`m` final).
/// Returns `None` for malformed input or button/event codes outside
/// Slice 6.0's clickable surface.
fn parse_sgr_params(params: &[u8], is_press: bool) -> Option<AppEvent> {
    let s = std::str::from_utf8(params).ok()?;
    let mut parts = s.split(';');
    let cb: u32 = parts.next()?.parse().ok()?;
    let cx: u32 = parts.next()?.parse().ok()?;
    let cy: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }

    // SGR rows/cols are 1-indexed. Convert to 0-indexed ratatui
    // cell coordinates; clamp into `u16` range.
    let x = u16::try_from(cx.saturating_sub(1)).ok()?;
    let y = u16::try_from(cy.saturating_sub(1)).ok()?;

    let motion = (cb & MOTION_BIT) != 0;
    let wheel = (cb & WHEEL_BIT) != 0;
    let button = cb & BUTTON_MASK;

    if motion {
        // Any-motion tracking: emit a hover regardless of button.
        // We don't currently distinguish drag from hover; the
        // clickable surface in Slice 6.0 is click-driven, not drag.
        Some(AppEvent::MouseHover { x, y })
    } else if wheel {
        // Wheel scroll — ignore for Slice 6.0.
        None
    } else if is_press && button == 0 {
        // Left-button press. We emit on press rather than release
        // because a user's "click an item" gesture registers on the
        // press edge in most UIs.
        Some(AppEvent::MouseClick { x, y })
    } else {
        // Right/middle press, left release, other — ignore.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_bytes_pass_through_unchanged() {
        let mut p = MouseParser::new();
        let (out, events) = p.parse(b"ls -la\n");
        assert_eq!(out, b"ls -la\n");
        assert!(events.is_empty());
    }

    #[test]
    fn sgr_left_click_emits_mouse_click_with_zero_indexed_coords() {
        // `\x1b[<0;11;6M` — left press at column 11, row 6 (1-indexed).
        let mut p = MouseParser::new();
        let (out, events) = p.parse(b"\x1b[<0;11;6M");
        assert!(out.is_empty(), "mouse bytes must be stripped");
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AppEvent::MouseClick { x, y } if *x == 10 && *y == 5
        ));
    }

    #[test]
    fn sgr_motion_emits_mouse_hover() {
        // cb=32 (motion bit only) → hover at (2, 3) [0-indexed].
        let mut p = MouseParser::new();
        let (_out, events) = p.parse(b"\x1b[<32;3;4M");
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AppEvent::MouseHover { x, y } if *x == 2 && *y == 3
        ));
    }

    #[test]
    fn sgr_left_release_is_ignored() {
        // Final `m` means release — we emit on press only.
        let mut p = MouseParser::new();
        let (out, events) = p.parse(b"\x1b[<0;10;5m");
        assert!(out.is_empty());
        assert!(events.is_empty());
    }

    #[test]
    fn sgr_right_click_is_ignored() {
        // cb=2 = right button — clickable surface is left-click only.
        let mut p = MouseParser::new();
        let (_out, events) = p.parse(b"\x1b[<2;5;5M");
        assert!(events.is_empty());
    }

    #[test]
    fn sgr_wheel_event_is_ignored() {
        // cb=64 = wheel up — not bound in Slice 6.0.
        let mut p = MouseParser::new();
        let (_out, events) = p.parse(b"\x1b[<64;5;5M");
        assert!(events.is_empty());
    }

    #[test]
    fn non_sgr_csi_flushes_raw_for_input_filter() {
        // Shift-Tab is `\x1b[Z` — MouseParser rejects at `Z` (not `<`)
        // and flushes the three bytes so InputFilter can parse them.
        let mut p = MouseParser::new();
        let (out, events) = p.parse(b"\x1b[Z");
        assert_eq!(out, b"\x1b[Z");
        assert!(events.is_empty());
    }

    #[test]
    fn bare_escape_flushes_when_non_bracket_follows() {
        // `\x1b a` — plain Esc then `a`. MouseParser flushes both.
        let mut p = MouseParser::new();
        let (out, events) = p.parse(b"\x1ba");
        assert_eq!(out, b"\x1ba");
        assert!(events.is_empty());
    }

    #[test]
    fn sgr_click_split_across_chunks() {
        let mut p = MouseParser::new();
        let (out1, events1) = p.parse(b"\x1b[<");
        assert!(out1.is_empty());
        assert!(events1.is_empty());
        let (out2, events2) = p.parse(b"0;7;3M");
        assert!(out2.is_empty());
        assert_eq!(events2.len(), 1);
        assert!(matches!(
            &events2[0],
            AppEvent::MouseClick { x, y } if *x == 6 && *y == 2
        ));
    }

    #[test]
    fn bytes_around_sgr_sequence_are_preserved() {
        let mut p = MouseParser::new();
        let (out, events) = p.parse(b"foo\x1b[<0;1;1Mbar");
        assert_eq!(out, b"foobar");
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AppEvent::MouseClick { x: 0, y: 0 }));
    }

    #[test]
    fn malformed_sgr_params_drop_silently() {
        // Non-numeric param — consumed until terminator, emits nothing.
        let mut p = MouseParser::new();
        let (out, events) = p.parse(b"\x1b[<notdigits;;;M");
        assert!(out.is_empty());
        assert!(events.is_empty());
    }

    #[test]
    fn coords_greater_than_u16_max_drop() {
        let mut p = MouseParser::new();
        let huge = format!("\x1b[<0;{};{}M", u32::MAX, u32::MAX);
        let (_out, events) = p.parse(huge.as_bytes());
        assert!(events.is_empty(), "coords outside u16 range must drop");
    }
}
