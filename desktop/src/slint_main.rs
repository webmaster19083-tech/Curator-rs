#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Generated from `ui/main.slint` by desktop/build.rs. The shell holds only
// immutable display state and command callbacks; application/session state
// remains in Curator's Rust services.
slint::include_modules!();

#[derive(Clone)]
struct NativeSessionViewModel {
    sessions: curator::session::SessionService,
}

/// Native screens emit this typed command rather than reaching into a
/// database, timer, selection algorithm, or browser API.
enum NativeCommand {
    StartQuickSession,
}

impl NativeSessionViewModel {
    fn execute(&self, command: NativeCommand) -> Result<curator::session::SessionState, String> {
        match command {
            NativeCommand::StartQuickSession => self
                .sessions
                .start_running(curator::session::GameConfig::quick_default())
                .map(|update| update.state),
        }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let window = CuratorNativeWindow::new()?;
    let view_model = NativeSessionViewModel {
        sessions: curator::session::SessionService::default(),
    };

    let weak_window = window.as_weak();
    window.on_select_page(move |page| {
        if let Some(window) = weak_window.upgrade() {
            window.set_active_page(page);
        }
    });

    let weak_window = window.as_weak();
    window.on_start_session(move || {
        if let Some(window) = weak_window.upgrade() {
            let message = match view_model.execute(NativeCommand::StartQuickSession) {
                Ok(snapshot) => format!(
                    "Quick session {} started at {:.0} BPM (seed {}).",
                    snapshot.session_id, snapshot.tempo.current_bpm, snapshot.seed
                ),
                Err(error) => error,
            };
            window.set_status(message.into());
        }
    });

    window.run()
}
