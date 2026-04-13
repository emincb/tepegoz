//! Detach prefix handling: `Ctrl-b` (0x02) followed by:
//!
//! - `d` / `q` → detach
//! - `s` → switch to scope view (Slice C+)
//! - `a` → switch back to attached pane (Slice C+)
//! - `?` → help overlay (Slice C2/C3)
//!
//! Any other byte after `Ctrl-b` — including another `Ctrl-b` — causes the
//! pending `Ctrl-b` to be forwarded as-is together with the next byte.

const PREFIX_BYTE: u8 = 0x02; // Ctrl-b
const DETACH_D: u8 = b'd';
const DETACH_Q: u8 = b'q';
const SWITCH_SCOPE: u8 = b's';
const SWITCH_PANE: u8 = b'a';
const HELP: u8 = b'?';

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InputAction {
    /// Forward these bytes — to the daemon as `SendInput` in pane mode, or
    /// to the scope-view key parser in scope mode. The filter doesn't know
    /// the active mode; the App routes based on `View`.
    Forward(Vec<u8>),
    /// User pressed the detach sequence; exit the attach loop.
    Detach,
    /// Switch to the scope view (`Ctrl-b s`).
    SwitchToScope,
    /// Switch back to the attached pane (`Ctrl-b a`).
    SwitchToPane,
    /// Open the help overlay (`Ctrl-b ?`).
    Help,
}

pub(crate) struct InputFilter {
    prefix_active: bool,
}

impl InputFilter {
    pub(crate) fn new() -> Self {
        Self {
            prefix_active: false,
        }
    }

    /// Process a chunk of raw input bytes from stdin, returning zero or more
    /// ordered actions. A control action (Detach / SwitchToScope /
    /// SwitchToPane / Help) is always the final action in a chunk that
    /// contains it; any bytes after that within the same chunk are dropped
    /// (the user is leaving / navigating anyway and we don't want the
    /// straggler bytes to leak to the new view).
    pub(crate) fn process(&mut self, input: &[u8]) -> Vec<InputAction> {
        let mut actions = Vec::new();
        let mut out = Vec::with_capacity(input.len());

        for &b in input {
            if self.prefix_active {
                self.prefix_active = false;
                let control = match b {
                    DETACH_D | DETACH_Q => Some(InputAction::Detach),
                    SWITCH_SCOPE => Some(InputAction::SwitchToScope),
                    SWITCH_PANE => Some(InputAction::SwitchToPane),
                    HELP => Some(InputAction::Help),
                    _ => None,
                };
                match control {
                    Some(action) => {
                        if !out.is_empty() {
                            actions.push(InputAction::Forward(std::mem::take(&mut out)));
                        }
                        actions.push(action);
                        return actions;
                    }
                    None => {
                        // Not a recognized prefix sequence — emit the
                        // pending Ctrl-b plus this byte and keep going.
                        out.push(PREFIX_BYTE);
                        out.push(b);
                    }
                }
            } else if b == PREFIX_BYTE {
                self.prefix_active = true;
            } else {
                out.push(b);
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
    fn ctrl_b_then_s_switches_to_scope() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02s");
        assert!(matches!(&a[..], [InputAction::SwitchToScope]));
    }

    #[test]
    fn ctrl_b_then_a_switches_to_pane() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02a");
        assert!(matches!(&a[..], [InputAction::SwitchToPane]));
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
    fn detach_splits_stream() {
        let mut f = InputFilter::new();
        let a = f.process(b"ls\n\x02d");
        assert_eq!(a.len(), 2);
        assert!(matches!(&a[0], InputAction::Forward(v) if v == b"ls\n"));
        assert!(matches!(&a[1], InputAction::Detach));
    }

    #[test]
    fn switch_to_scope_splits_stream() {
        let mut f = InputFilter::new();
        let a = f.process(b"ls\n\x02s");
        assert_eq!(a.len(), 2);
        assert!(matches!(&a[0], InputAction::Forward(v) if v == b"ls\n"));
        assert!(matches!(&a[1], InputAction::SwitchToScope));
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
    fn ctrl_b_then_s_split_across_chunks() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02");
        assert!(a.is_empty());
        let b = f.process(b"s");
        assert!(matches!(&b[..], [InputAction::SwitchToScope]));
    }

    #[test]
    fn ctrl_b_ctrl_b_emits_both() {
        // First Ctrl-b enters prefix, second Ctrl-b is a regular byte after
        // prefix, so we emit Ctrl-b + Ctrl-b.
        let mut f = InputFilter::new();
        let a = f.process(b"\x02\x02");
        assert!(matches!(&a[..], [InputAction::Forward(v)] if v == b"\x02\x02"));
    }
}
