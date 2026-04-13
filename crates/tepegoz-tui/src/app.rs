//! TUI app state.

use crossterm::event::KeyCode;
use tepegoz_proto::StatusSnapshot;

pub struct App {
    pub connection: ConnectionState,
    pub last_status: Option<StatusSnapshot>,
}

impl App {
    pub fn new() -> Self {
        Self {
            connection: ConnectionState::Connecting,
            last_status: None,
        }
    }
}

#[derive(Clone)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Disconnected(String),
}

pub enum AppEvent {
    Key(KeyCode),
    Redraw,
    ConnectionState(ConnectionState),
    Status(StatusSnapshot),
    ConnectionLost(String),
}
