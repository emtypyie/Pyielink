use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc, OnceLock};
use std::thread;

static INPUT_TX: OnceLock<mpsc::Sender<crate::client::DlCommand>> = OnceLock::new();
static INPUT_RUNNING: OnceLock<Arc<AtomicBool>> = OnceLock::new();

pub fn start_input_capture(
    running: Arc<AtomicBool>,
    tx: mpsc::Sender<crate::client::DlCommand>,
) -> thread::JoinHandle<()> {
    let _ = INPUT_TX.set(tx.clone());
    let _ = INPUT_RUNNING.set(running.clone());

    // Input capture is handled by the Node.js InputService (inject_linux.js / inject_mac.js)
    // on non-Windows platforms. On Windows, the low-level keyboard/mouse hooks would be
    // installed here, but the GUI wrapper delegates to the JS side for all platforms.
    // This function keeps the API compatible but does not install platform-specific hooks.
    let _ = (running, tx);
    thread::spawn(|| {})
}