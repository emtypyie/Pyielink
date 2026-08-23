//! Minimal video viewer window for the GUI session.
//!
//! The data layer streams the remote screen as H.264 inside an MPEG-TS
//! container. We decode it with the system `ffmpeg` into raw BGR24 frames
//! and blit those frames onto a Win32 window using GDI.

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

struct WindowState {
    frame: std::sync::Mutex<Option<Vec<u8>>>,
}

/// Handle returned to the caller. Feed MPEG-TS chunks via `push_ts`, and the
/// window decodes + renders them.
pub struct VideoWindow {
    frame_tx: std::sync::mpsc::Sender<Vec<u8>>,
    hwnd: Arc<std::sync::Mutex<Option<isize>>>,
    decoder: Option<Decoder>,
    thread: Option<thread::JoinHandle<()>>,
}

struct Decoder {
    stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
    child: std::process::Child,
}

impl VideoWindow {
    pub fn new(title: &str) -> Self {
        let (frame_tx, frame_rx) = channel::<Vec<u8>>();
        let hwnd_slot = Arc::new(std::sync::Mutex::new(None));

        let mut child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-fflags",
                "+nobuffer",
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
            .expect("failed to spawn ffmpeg for video decode");

        let stdin = child.stdin.take().expect("ffmpeg stdin");
        let stdout = child.stdout.take().expect("ffmpeg stdout");

        let decoder = Decoder {
            stdin,
            stdout,
            child,
        };

        let title = title.to_string();
        let hwnd_slot_clone = hwnd_slot.clone();
        let thread = thread::spawn(move || run_window(&title, frame_rx, hwnd_slot_clone));

        VideoWindow {
            frame_tx,
            hwnd: hwnd_slot,
            decoder: Some(decoder),
            thread: Some(thread),
        }
    }

    /// Feed an MPEG-TS chunk from the data link into the decoder.
    pub fn push_ts(&mut self, chunk: &[u8]) {
        if let Some(dec) = self.decoder.as_mut() {
            let _ = dec.stdin.write_all(chunk);
        }
    }

    /// Drain any decoded frames from ffmpeg and forward to the window.
    /// Call this after each `push_ts`.
    pub fn pump(&mut self) {
        if let Some(dec) = self.decoder.as_mut() {
            let mut buf = vec![0u8; FRAME_BYTES];
            if read_exact_nonblock(&mut dec.stdout, &mut buf).is_ok() {
                let _ = self.frame_tx.send(buf);
            }
        }
    }
}

impl Drop for VideoWindow {
    fn drop(&mut self) {
        if let Some(mut dec) = self.decoder.take() {
            let _ = dec.stdin.write_all(b"q");
            let _ = dec.child.kill();
        }
        // Ask the window thread to quit so join() can return.
        if let Some(h) = *self.hwnd.lock().unwrap() {
            unsafe {
                let _ = PostMessageW(HWND(h as *mut core::ffi::c_void), WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Read exactly `buf.len()` bytes; return Err if a full frame is not yet
/// available (non-fatal — we simply skip this pump).
fn read_exact_nonblock(r: &mut impl Read, buf: &mut [u8]) -> Result<(), ()> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => return Err(()),
            Ok(n) => filled += n,
            Err(_) => return Err(()),
        }
        if filled < buf.len() {
            thread::yield_now();
        }
    }
    Ok(())
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
                if let Ok(guard) = state.frame.lock() {
                    if let Some(frame) = guard.as_ref() {
                        paint_frame(hwnd, frame);
                    }
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
) {
    let state = Arc::new(WindowState {
        frame: std::sync::Mutex::new(None),
    });

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
