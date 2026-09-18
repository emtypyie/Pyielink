//! Thin launcher for the pyielink file-explorer GUI.
//!
//! The full explorer lives in `pyielink::gui` (feature `gui` in pyielink-rs);
//! this crate just boots it with a target `user@ip`.
//!
//! The old video viewer is gone: the video layer is broken and was removed
//! from the framework. This is a pure remote+local file explorer.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: pyielink-gui <user@ip>");
        std::process::exit(1);
    }
    let target = args[0].clone();
    if let Err(e) = pyielink::gui::run_explorer(&target) {
        eprintln!("gui error: {}", e);
        std::process::exit(1);
    }
}