use crate::client::InputEvent;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc, OnceLock};
use std::thread;
use std::ptr;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_MOUSEMOVE, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN,
    WM_XBUTTONDOWN, WM_XBUTTONUP, KBDLLHOOKSTRUCT, MSLLHOOKSTRUCT,
};

static INPUT_TX: OnceLock<mpsc::Sender<crate::client::DlCommand>> = OnceLock::new();
static INPUT_RUNNING: OnceLock<Arc<AtomicBool>> = OnceLock::new();

pub fn start_input_capture(
    running: Arc<AtomicBool>,
    tx: mpsc::Sender<crate::client::DlCommand>,
) -> thread::JoinHandle<()> {
    let _ = INPUT_TX.set(tx);
    let _ = INPUT_RUNNING.set(running.clone());

    thread::spawn(move || {
        unsafe {
            let hook_keyboard = SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(keyboard_hook_proc),
                None,
                0,
            );
            let hook_mouse = SetWindowsHookExW(
                WH_MOUSE_LL,
                Some(mouse_hook_proc),
                None,
                0,
            );

            let hook_keyboard = match hook_keyboard {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("[input] failed to set keyboard hook: {}", e);
                    return;
                }
            };
            let hook_mouse = match hook_mouse {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("[input] failed to set mouse hook: {}", e);
                    return;
                }
            };

            println!("[input] capture started");
            let mut msg = std::mem::zeroed();
            while running.load(Ordering::Relaxed) && GetMessageW(&mut msg, HWND(ptr::null_mut()), 0, 0).into() {
            }

            let _ = UnhookWindowsHookEx(hook_keyboard);
            let _ = UnhookWindowsHookEx(hook_mouse);
            println!("[input] capture stopped");
        }
    })
}

unsafe extern "system" fn keyboard_hook_proc(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    if n_code >= 0 {
        if let Some(tx) = INPUT_TX.get() {
            if let Some(running) = INPUT_RUNNING.get() {
                if running.load(Ordering::Relaxed) {
                    let kbd = &*(l_param.0 as *const KBDLLHOOKSTRUCT);
                    let vk = kbd.vkCode as u16;
                    let scan = kbd.scanCode as u16;
                    let flags = kbd.flags.0 as u32;

                    let event = if w_param.0 as u32 == WM_KEYDOWN || w_param.0 as u32 == WM_SYSKEYDOWN {
                        InputEvent::KeyDown { vk, scan, flags }
                    } else {
                        InputEvent::KeyUp { vk, scan, flags: flags | 0x0002 }
                    };

                    let _ = tx.send(crate::client::DlCommand::Input { events: vec![event] });
                }
            }
        }
    }
    CallNextHookEx(None, n_code, w_param, l_param)
}

unsafe extern "system" fn mouse_hook_proc(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    if n_code >= 0 {
        if let Some(tx) = INPUT_TX.get() {
            if let Some(running) = INPUT_RUNNING.get() {
                if running.load(Ordering::Relaxed) {
                    let mouse = &*(l_param.0 as *const MSLLHOOKSTRUCT);
                    let x = mouse.pt.x;
                    let y = mouse.pt.y;
                    let mouse_data = mouse.mouseData;

                    let event = match w_param.0 as u32 {
                        WM_MOUSEMOVE => InputEvent::MouseMove { x, y, flags: 0 },
                        WM_LBUTTONDOWN => InputEvent::MouseDown { button: 0, x, y, flags: 0 },
                        WM_LBUTTONUP => InputEvent::MouseUp { button: 0, x, y, flags: 0 },
                        WM_RBUTTONDOWN => InputEvent::MouseDown { button: 1, x, y, flags: 0 },
                        WM_RBUTTONUP => InputEvent::MouseUp { button: 1, x, y, flags: 0 },
                        WM_MOUSEWHEEL => {
                            let delta = ((mouse_data >> 16) as i16) as i32;
                            InputEvent::MouseWheel { delta, x, y, flags: 0 }
                        }
                        WM_XBUTTONDOWN => {
                            let button = if (mouse_data & 0xFFFF) == 1 { 3 } else { 4 };
                            InputEvent::MouseDown { button, x, y, flags: 0 }
                        }
                        WM_XBUTTONUP => {
                            let button = if (mouse_data & 0xFFFF) == 1 { 3 } else { 4 };
                            InputEvent::MouseUp { button, x, y, flags: 0 }
                        }
                        _ => {
                            return CallNextHookEx(None, n_code, w_param, l_param);
                        }
                    };

                    let _ = tx.send(crate::client::DlCommand::Input { events: vec![event] });
                }
            }
        }
    }
    CallNextHookEx(None, n_code, w_param, l_param)
}