//! Detach prefix handling: `Ctrl-b` (0x02) followed by `d` or `q` detaches.
//!
//! Any other byte after `Ctrl-b` — including another `Ctrl-b` — causes the
//! pending `Ctrl-b` to be forwarded as-is together with the next byte.

const PREFIX_BYTE: u8 = 0x02; // Ctrl-b
const DETACH_D: u8 = b'd';
const DETACH_Q: u8 = b'q';

#[derive(Debug)]
pub(crate) enum InputAction {
    /// Forward these bytes to the daemon as `SendInput`.
    Forward(Vec<u8>),
    /// User pressed the detach sequence; exit the attach loop.
    Detach,
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
    /// ordered actions. `Detach` is always the final action in a chunk that
    /// contains it; any bytes after detach within the same chunk are dropped
    /// (the user is leaving anyway).
    pub(crate) fn process(&mut self, input: &[u8]) -> Vec<InputAction> {
        let mut actions = Vec::new();
        let mut out = Vec::with_capacity(input.len());

        for &b in input {
            if self.prefix_active {
                self.prefix_active = false;
                match b {
                    DETACH_D | DETACH_Q => {
                        if !out.is_empty() {
                            actions.push(InputAction::Forward(std::mem::take(&mut out)));
                        }
                        actions.push(InputAction::Detach);
                        return actions;
                    }
                    _ => {
                        // Not a detach suffix — emit the pending Ctrl-b plus
                        // this byte and keep going.
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
    fn ctrl_b_split_across_chunks() {
        let mut f = InputFilter::new();
        let a = f.process(b"\x02");
        assert!(a.is_empty());
        let b = f.process(b"d");
        assert!(matches!(&b[..], [InputAction::Detach]));
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
