//! Minimal video viewer window for the GUI session.
//!
//! The data layer streams the remote screen as H.264 inside an MPEG-TS
//! container. We decode it with the system `ffmpeg` into raw BGR24 frames
//! and blit those frames onto a Win32 window using GDI.
//!
//! Threading model (nothing here may ever block the network reader):
//!   feeder thread   : TS chunks from `push_ts` -> ffmpeg stdin
//!   renderer thread : ffmpeg stdout -> exact raw frames -> frame channel
//!   paint thread    : Win32 message loop, blits latest frame or status text

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::thread;

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WINDOW_WIDTH: i32 = 1280;
pub const WINDOW_HEIGHT: i32 = 720;
const FRAME_BYTES: usize = (WINDOW_WIDTH as usize) * (WINDOW_HEIGHT as usize) * 3;

const CONNECTING_TEXT: &str = "Connecting…";

struct WindowState {
    frame: std::sync::Mutex<Option<Vec<u8>>>,
    /// Shown centered until/unless frames are flowing (also used for errors).
    status: std::sync::Mutex<String>,
}

fn set_shared_status(state: &Arc<WindowState>, hwnd_slot: &Arc<std::sync::Mutex<Option<isize>>>, text: &str) {
    if let Ok(mut guard) = state.status.lock() {
        *guard = text.to_string();
    }
    if let Ok(guard) = hwnd_slot.lock() {
        if let Some(h) = *guard {
            unsafe {
                let hh = HWND(h as *mut core::ffi::c_void);
                let _ = InvalidateRect(hh, None, false);
                let _ = UpdateWindow(hh);
            }
        }
    }
}

/// Handle returned to the caller. Feed MPEG-TS chunks via `push_ts`; two
/// dedicated threads move bytes into ffmpeg and decoded frames out, so a
/// slow/stalled decode can never block the network reader thread.
pub struct VideoWindow {
    ts_tx: std::sync::mpsc::Sender<Vec<u8>>,
    hwnd: Arc<std::sync::Mutex<Option<isize>>>,
    state: Arc<WindowState>,
    child: std::process::Child,
    feeder: Option<thread::JoinHandle<()>>,
    renderer: Option<thread::JoinHandle<()>>,
    paint: Option<thread::JoinHandle<()>>,
}

impl VideoWindow {
    /// Creates the window + ffmpeg decoder. Fails (without panicking) when
    /// `ffmpeg` is unavailable; the caller can then continue headless.
    pub fn new(title: &str) -> Result<Self, String> {
        let (frame_tx, frame_rx) = channel::<Vec<u8>>();
        let hwnd_slot = Arc::new(std::sync::Mutex::new(None));
        let state = Arc::new(WindowState {
            frame: std::sync::Mutex::new(None),
            status: std::sync::Mutex::new(CONNECTING_TEXT.to_string()),
        });

        let mut child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-fflags",
                "+nobuffer",
                // Input options: skip ffmpeg's multi-MB stream probe so the
                // first frame paints immediately instead of seconds later.
                "-probesize",
                "32",
                "-analyzeduration",
                "0",
                "-f",
                "mpegts",
                "-i",
                "pipe:0",
                "-vf",
                &format!("scale={}:{}", WINDOW_WIDTH, WINDOW_HEIGHT),
                "-pix_fmt",
                "bgr24",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("ffmpeg not available for video decode: {}", e))?;

        let mut stdin = child.stdin.take().ok_or("ffmpeg stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("ffmpeg stdout unavailable")?;

        // Feeder: network side enqueues TS chunks; this thread owns stdin so
        // a full pipe can never block the caller.
        let (ts_tx, ts_rx) = channel::<Vec<u8>>();
        let feeder = thread::spawn(move || {
            for chunk in ts_rx.iter() {
                if stdin.write_all(&chunk).is_err() {
                    break;
                }
            }
        });

        // Renderer: blocking read of exactly one raw frame at a time. On
        // decoder death the window shows the reason instead of freezing.
        let state_for_renderer = state.clone();
        let hwnd_for_renderer = hwnd_slot.clone();
        let renderer = thread::spawn(move || {
            let mut reader = stdout;
            let mut frame = vec![0u8; FRAME_BYTES];
            loop {
                match reader.read_exact(&mut frame) {
                    Ok(()) => {
                        if frame_tx.send(frame.clone()).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let msg = format!("Video decoder stopped ({e})");
                        eprintln!("[video-window] {msg}");
                        set_shared_status(&state_for_renderer, &hwnd_for_renderer, &msg);
                        break;
                    }
                }
            }
        });

        let title = title.to_string();
        let hwnd_slot_clone = hwnd_slot.clone();
        let state_for_window = state.clone();
        let paint = thread::spawn(move || {
            run_window(&title, frame_rx, hwnd_slot_clone, state_for_window)
        });

        Ok(VideoWindow {
            ts_tx,
            hwnd: hwnd_slot,
            state,
            child,
            feeder: Some(feeder),
            renderer: Some(renderer),
            paint: Some(paint),
        })
    }

    /// Feed an MPEG-TS chunk from the data link into the decoder. Never
    /// blocks on ffmpeg: chunks are queued and the feeder thread writes them.
    pub fn push_ts(&mut self, chunk: &[u8]) {
        let _ = self.ts_tx.send(chunk.to_vec());
    }

    /// Update the placeholder/error text shown while no frame is available.
    pub fn set_status(&self, text: &str) {
        set_shared_status(&self.state, &self.hwnd, text);
    }

    /// Backwards-compatible no-op: draining now happens continuously on the
    /// renderer thread instead of being polled by the caller.
    pub fn pump(&mut self) {}
}

impl Drop for VideoWindow {
    fn drop(&mut self) {
        // Dropping ts_tx closes the channel → feeder exits → stdin closes →
        // ffmpeg flushes and terminates.
        if let Some(h) = *self.hwnd.lock().unwrap() {
            unsafe {
                let _ = PostMessageW(HWND(h as *mut core::ffi::c_void), WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(t) = self.feeder.take() {
            let _ = t.join();
        }
        if let Some(t) = self.renderer.take() {
            let _ = t.join();
        }
        if let Some(t) = self.paint.take() {
            let _ = t.join();
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowState;
            if !ptr.is_null() {
                let state = &*ptr;
                let has_frame = state
                    .frame
                    .lock()
                    .ok()
                    .map(|g| g.is_some())
                    .unwrap_or(false);
                if has_frame {
                    if let Ok(guard) = state.frame.lock() {
                        if let Some(frame) = guard.as_ref() {
                            paint_frame(hwnd, frame);
                        }
                    }
                } else {
                    let text = state
                        .status
                        .lock()
                        .map(|g| g.clone())
                        .unwrap_or_default();
                    paint_status(hwnd, &text);
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Placeholder/error screen (white background + centered status text) drawn
/// until the first decoded video frame arrives.
fn paint_status(hwnd: HWND, text: &str) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_invalid() {
            return;
        }
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let brush = HBRUSH(GetStockObject(WHITE_BRUSH).0);
        let _ = FillRect(hdc, &rc, brush);
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(0x00646058));
        let old = SelectObject(hdc, GetStockObject(DEFAULT_GUI_FONT));
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        if !wide.is_empty() {
            let _ = DrawTextW(
                hdc,
                &mut wide,
                &mut rc,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
        }
        SelectObject(hdc, old);
        EndPaint(hwnd, &ps);
    }
}

fn paint_frame(hwnd: HWND, frame: &[u8]) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_invalid() {
            return;
        }

        let hdc_bmp = CreateCompatibleDC(hdc);
        if hdc_bmp.is_invalid() {
            EndPaint(hwnd, &ps);
            return;
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: WINDOW_WIDTH,
                biHeight: -WINDOW_HEIGHT, // top-down
                biPlanes: 1,
                biBitCount: 24,
                biCompression: BI_RGB.0 as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        let ptr = frame.as_ptr() as *const core::ffi::c_void;
        let _ = SetDIBitsToDevice(
            hdc_bmp,
            0,
            0,
            WINDOW_WIDTH as u32,
            WINDOW_HEIGHT as u32,
            0,
            0,
            0,
            WINDOW_HEIGHT as u32,
            ptr,
            &bmi,
            DIB_RGB_COLORS,
        );
        let _ = BitBlt(
            hdc,
            0,
            0,
            WINDOW_WIDTH,
            WINDOW_HEIGHT,
            hdc_bmp,
            0,
            0,
            SRCCOPY,
        );
        let _ = DeleteDC(hdc_bmp);

        EndPaint(hwnd, &ps);
    }
}

fn run_window(
    title: &str,
    frame_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    hwnd_slot: Arc<std::sync::Mutex<Option<isize>>>,
    state: Arc<WindowState>,
) {
    let class_name: Vec<u16> = "PyielinkVideoClass\0".encode_utf16().collect();
    let title_w: Vec<u16> = format!("{}\0", title).encode_utf16().collect();

    unsafe {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(GetModuleHandleW(None).unwrap().0),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            WINDOW_WIDTH,
            WINDOW_HEIGHT,
            None,
            None,
            wc.hInstance,
            None,
        );

        if let Ok(hwnd) = hwnd {
            *hwnd_slot.lock().unwrap() = Some(hwnd.0 as isize);
            let state_ptr = Arc::into_raw(state.clone()) as isize;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr);

            // Frame pump: receive decoded frames and trigger repaints.
            let hwnd_for_pump = hwnd.0 as isize;
            thread::spawn(move || {
                while let Ok(frame) = frame_rx.recv() {
                    if let Ok(mut guard) = state.frame.lock() {
                        *guard = Some(frame);
                    }
                    let h = HWND(hwnd_for_pump as *mut core::ffi::c_void);
                    let _ = InvalidateRect(h, None, false);
                    let _ = UpdateWindow(h);
                }
                // Reconstruct the Arc stored in the window to drop it.
                unsafe {
                    let _ = Arc::from_raw(state_ptr as *const WindowState);
                }
            });

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).into() {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}
