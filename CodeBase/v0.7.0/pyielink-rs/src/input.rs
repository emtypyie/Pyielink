use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc, OnceLock};
use std::thread;

static INPUT_TX: OnceLock<mpsc::Sender<crate::client::DlCommand>> = OnceLock::new();
static INPUT_RUNNING: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Start platform-specific input capture.
/// On Windows, the Node.js InputService spawns `assets/inject.ps1`
/// (a PowerShell helper with a compiled SendInput P/Invoke) which
/// reads JSON events from stdin. This function signals the start.
/// On non-Windows, the Node.js inject_linux.js / inject_mac.js handles it.
pub fn start_input_capture(
    running: Arc<AtomicBool>,
    tx: mpsc::Sender<crate::client::DlCommand>,
) -> thread::JoinHandle<()> {
    let _ = INPUT_TX.set(tx.clone());
    let _ = INPUT_RUNNING.set(running.clone());
    thread::spawn(move || {
        // Input events flow: client → data-link mux → Node.js InputService →
        // platform injector (inject.ps1 / inject_linux.js / inject_mac.js).
        // The Rust side signals start/stop via DlCommand::InputStart/InputStop.
        while running.load(Ordering::Relaxed) {
            std::thread::park();
        }
    })
}

/// Signal that input capture should stop.
pub fn stop_input_capture() {
    if let Some(tx) = INPUT_TX.get() {
        let _ = tx.send(crate::client::DlCommand::InputStop);
    }
}