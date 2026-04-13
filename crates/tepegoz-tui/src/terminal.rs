//! Terminal raw-mode setup and RAII guard.

use std::io::Write;

pub(crate) fn enter_raw(title: &str) -> anyhow::Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    // OSC 0 sets both icon name and window title. Gives the user an
    // unambiguous signal that this terminal is an attached tepegoz pane,
    // since the pane's shell prompt is otherwise indistinguishable from
    // the outer shell's.
    let _ = write!(stdout, "\x1b]0;{title}\x07");
    let _ = stdout.flush();
    Ok(())
}

/// Dropping this restores the terminal — safe under panic, early return, or
/// normal exit.
pub(crate) struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        // Clear the title we set on entry. Most terminals then fall back
        // to the shell's own title updates.
        let _ = write!(stdout, "\x1b]0;\x07");
        let _ = stdout.flush();
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(stdout, crossterm::terminal::LeaveAlternateScreen);
    }
}
