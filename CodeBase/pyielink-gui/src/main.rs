use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use eframe::egui::{self, RichText, Color32};
use pyielink::client::{run_gui_session, InputEvent, video_control_sender, DlCommand};

mod video_decoder;
mod file_browser;
use video_decoder::VideoDecoder;
use file_browser::{FileBrowser, render_file_browser, RemoteFileEntry};

#[derive(Debug, Clone)]
struct MonitorInfo {
    index: u32,
    name: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    is_primary: bool,
}

fn enumerate_monitors() -> Vec<MonitorInfo> {
    // For now, return a simple default monitor
    // TODO: Implement proper Windows monitor enumeration
    vec![MonitorInfo {
        index: 0,
        name: "Primary Monitor".to_string(),
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        is_primary: true,
    }]
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: pyielink-gui <user@ip>");
        std::process::exit(1);
    }
    let target = args[0].clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
            .with_title(format!("PYIELINK FRAMEWORK  {}", target)),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "pyielink-gui",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(ViewerApp::new(cc, target)))
        }),
    ).map_err(|e| anyhow::anyhow!("eframe error: {:?}", e))?;

    Ok(())
}

struct ViewerApp {
    target: String,
    connected: bool,
    video_texture: Option<egui::TextureHandle>,
    decoder: Arc<std::sync::Mutex<VideoDecoder>>,
    input_running: bool,
    monitors: Vec<MonitorInfo>,
    selected_monitor: usize,
    show_monitor_selector: bool,
    monitor_offset_x: i32,
    monitor_offset_y: i32,
    monitor_width: u32,
    monitor_height: u32,
    video_ctrl_tx: Option<std::sync::mpsc::Sender<DlCommand>>,
    has_focus: bool,
    file_browser: Option<FileBrowser>,
    xfer_tx: Option<std::sync::mpsc::Sender<DlCommand>>,
    show_file_browser: bool,
    // OSD state
    osd_visible: bool,
    fps: f32,
    bitrate_kbps: u32,
    rtt_ms: u128,
    input_active: bool,
    frames_dropped: u64,
    frames_duplicated: u64,
    last_frame_time: Instant,
    frame_count: u32,
    last_fps_update: Instant,
}

impl ViewerApp {
    fn new(cc: &eframe::CreationContext<'_>, target: String) -> Self {
        let monitors = enumerate_monitors();
        let primary_index = monitors.iter().position(|m| m.is_primary).unwrap_or(0);
        
        let (offset_x, offset_y, width, height) = if let Some(m) = monitors.get(primary_index) {
            (m.x, m.y, m.width, m.height)
        } else {
            (0, 0, 1920, 1080)
        };

        // Get video control sender for focus-gated capture
        let video_ctrl_tx = video_control_sender();
        
        // Create file browser with current directory as root
        let local_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let file_browser = FileBrowser::new(local_root);

        let now = Instant::now();
        Self {
            target,
            connected: false,
            video_texture: None,
            decoder: Arc::new(std::sync::Mutex::new(VideoDecoder::new().expect("Failed to create video decoder"))),
            input_running: false,
            monitors,
            selected_monitor: primary_index,
            show_monitor_selector: true,
            monitor_offset_x: offset_x,
            monitor_offset_y: offset_y,
            monitor_width: width,
            monitor_height: height,
            video_ctrl_tx,
            has_focus: true,
            file_browser: Some(file_browser),
            xfer_tx: None,
            show_file_browser: false,
            // OSD state
            osd_visible: true,
            fps: 0.0,
            bitrate_kbps: 0,
            rtt_ms: 0,
            input_active: false,
            frames_dropped: 0,
            frames_duplicated: 0,
            last_frame_time: Instant::now(),
            frame_count: 0,
            last_fps_update: Instant::now(),
        }
    }

    fn connect(&mut self) {
        if self.connected {
            return;
        }
        let target = self.target.clone();
        let decoder = self.decoder.clone();
        let monitor_index = self.selected_monitor as u32;
        let offset_x = self.monitor_offset_x;
        let offset_y = self.monitor_offset_y;
        let width = self.monitor_width;
        let height = self.monitor_height;

        let (xfer_tx, xfer_rx) = std::sync::mpsc::channel::<DlCommand>();
        self.xfer_tx = Some(xfer_tx.clone());

        std::thread::spawn(move || {
            let video_cb = Box::new(move |payload: &[u8]| {
                if let Ok(mut dec) = decoder.lock() {
                    let _ = dec.feed(payload);
                }
            });
            let audio_cb = Box::new(move |_payload: &[u8]| {
                // Audio playback would go here
            });
            // Note: run_gui_session doesn't yet accept monitor parameters
            // We'll need to update the client to pass them
            let _ = run_gui_session(&target, Some(video_cb), Some(audio_cb));
        });

        // Spawn file transfer handler thread
        let xfer_rx = xfer_rx;
        std::thread::spawn(move || {
            while let Ok(cmd) = xfer_rx.recv() {
                // The actual file transfer is handled by the client's data_link_loop
                // This channel just forwards commands to the client
                // The actual implementation would need to forward to the data link
            }
        });

        self.connected = true;
    }

fn toggle_input(&mut self) {
        self.input_running = !self.input_running;
        self.input_active = self.input_running;
        if let Some(ref tx) = self.video_ctrl_tx {
            if self.input_running {
                let _ = tx.send(DlCommand::InputStart);
            } else {
                let _ = tx.send(DlCommand::InputStop);
            }
        }
    }

    fn render_osd_overlay(&mut self, ui: &mut egui::Ui, video_rect: &egui::Rect) {
        // Update FPS counter
        let now = Instant::now();
        self.frame_count += 1;
        if now.duration_since(self.last_fps_update).as_secs_f32() >= 1.0 {
            self.fps = self.frame_count as f32 / now.duration_since(self.last_fps_update).as_secs_f32();
            self.frame_count = 0;
            self.last_fps_update = now;
            
            // Update decoder stats if available
            if let Ok(decoder) = self.decoder.lock() {
                let (dropped, duplicated) = decoder.frame_stats();
                self.frames_dropped = dropped;
                self.frames_duplicated = duplicated;
            }
        }
        
        // Render OSD in top-left corner of video
        let osd_rect = egui::Rect::from_min_size(
            video_rect.min + egui::vec2(10.0, 10.0),
            egui::vec2(280.0, 160.0)
        );
        
        egui::Area::new("osd_overlay".into())
            .fixed_pos(osd_rect.min)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::none()
                    .fill(egui::Color32::from_rgba_premultiplied(0, 0, 0, 180))
                    .rounding(egui::Rounding::same(6.0))
                    .inner_margin(egui::Margin::same(8.0))
                    .show(ui, |ui| {
                        ui.set_min_width(260.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new("PYIELINK OSD").strong().size(14.0).color(Color32::from_rgb(100, 255, 100)));
                            ui.separator();
                            
                            // FPS
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("FPS:").size(12.0));
                                let fps_color = if self.fps >= 25.0 { Color32::GREEN } else if self.fps >= 15.0 { Color32::YELLOW } else { Color32::RED };
                                ui.label(RichText::new(format!("{:.1}", self.fps)).size(12.0).color(fps_color).strong());
                            });
                            
                            // Bitrate
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Bitrate:").size(12.0));
                                ui.label(RichText::new(format!("{} kbps", self.bitrate_kbps)).size(12.0).strong());
                            });
                            
                            // RTT
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("RTT:").size(12.0));
                                let rtt_color = if self.rtt_ms < 50 { Color32::GREEN } else if self.rtt_ms < 100 { Color32::YELLOW } else { Color32::RED };
                                ui.label(RichText::new(format!("{} ms", self.rtt_ms)).size(12.0).color(rtt_color).strong());
                            });
                            
                            // Input status
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Input:").size(12.0));
                                let (status_text, status_color) = if self.input_active {
                                    ("ACTIVE", Color32::GREEN)
                                } else {
                                    ("INACTIVE", Color32::GRAY)
                                };
                                ui.label(RichText::new(status_text).size(12.0).color(status_color).strong());
                            });
                            
                            // Frame stats
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Dropped:").size(12.0));
                                ui.label(RichText::new(format!("{}", self.frames_dropped)).size(12.0).color(Color32::RED).strong());
                                ui.label(RichText::new("  Dup:").size(12.0));
                                ui.label(RichText::new(format!("{}", self.frames_duplicated)).size(12.0).color(Color32::YELLOW).strong());
                            });
                            
                            // Connection status
                            ui.separator();
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Status:").size(12.0));
                                let (conn_text, conn_color) = if self.connected {
                                    ("CONNECTED", Color32::GREEN)
                                } else {
                                    ("DISCONNECTED", Color32::RED)
                                };
                                ui.label(RichText::new(conn_text).size(12.0).color(conn_color).strong());
                            });
                            
                            // Toggle hint
                            ui.separator();
                            ui.label(RichText::new("[F1] Toggle OSD  [F2] Toggle Input").size(10.0).color(Color32::GRAY));
                        });
                    });
            });
    }
}
impl eframe::App for ViewerApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if !self.connected && !self.show_monitor_selector {
            self.connect();
        }

        // Show monitor selector before connecting
        if self.show_monitor_selector {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.heading("Select Monitor");
                    ui.add_space(20.0);
                    
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for (i, monitor) in self.monitors.iter().enumerate() {
                            let is_selected = i == self.selected_monitor;
                            let label = if monitor.is_primary {
                                format!("{} (Primary) - {}x{} at ({}, {})", 
                                    monitor.name, monitor.width, monitor.height, monitor.x, monitor.y)
                            } else {
                                format!("{} - {}x{} at ({}, {})", 
                                    monitor.name, monitor.width, monitor.height, monitor.x, monitor.y)
                            };
                            
                            if ui.selectable_label(is_selected, label).clicked() {
                                self.selected_monitor = i;
                                self.monitor_offset_x = monitor.x;
                                self.monitor_offset_y = monitor.y;
                                self.monitor_width = monitor.width;
                                self.monitor_height = monitor.height;
                            }
                        }
                    });
                    
                    ui.add_space(20.0);
                    
                    // Manual override
                    ui.group(|ui| {
                        ui.label("Manual Override (optional):");
                        ui.horizontal(|ui| {
                            ui.label("Offset X:");
                            ui.add(egui::DragValue::new(&mut self.monitor_offset_x).speed(1));
                            ui.label("Offset Y:");
                            ui.add(egui::DragValue::new(&mut self.monitor_offset_y).speed(1));
                        });
                        ui.horizontal(|ui| {
                            ui.label("Width:");
                            ui.add(egui::DragValue::new(&mut self.monitor_width).speed(1).clamp_range(1..=7680));
                            ui.label("Height:");
                            ui.add(egui::DragValue::new(&mut self.monitor_height).speed(1).clamp_range(1..=4320));
                        });
                    });
                    
                    ui.add_space(20.0);
                    
                    if ui.button("Connect").clicked() {
                        self.show_monitor_selector = false;
                    }
                });
            });
        } else if !self.connected {
            self.connect();
        }

        // Focus-gated capture: pause/resume video based on window focus
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if focused != self.has_focus {
            self.has_focus = focused;
            if let Some(ref tx) = self.video_ctrl_tx {
                if focused {
                    let _ = tx.send(DlCommand::VideoResume);
                } else {
                    let _ = tx.send(DlCommand::VideoPause);
                }
            }
        }

        // Top toolbar
        egui::TopBottomPanel::top("top_toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("PYIELINK FRAMEWORK").strong().size(14.0));
                ui.separator();
                
                // File browser toggle
                let fb_text = if self.show_file_browser { "📁 Hide File Browser" } else { "📁 Show File Browser" };
                if ui.button(fb_text).clicked() {
                    self.show_file_browser = !self.show_file_browser;
                }
                
                ui.separator();
                
                // Input toggle
                let input_text = if self.input_running { "⌨ Stop Input Capture" } else { "⌨ Start Input Capture" };
                if ui.button(input_text).clicked() {
                    self.toggle_input();
                }
                
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("🔄 Refresh").clicked() {
                        if let Some(browser) = &mut self.file_browser {
                            let _ = browser.local_root.load_children();
                        }
                    }
                });
            });
        });

        // File browser side panel
        if self.show_file_browser {
            if let Some(browser) = &mut self.file_browser {
                if let Some(xfer_tx) = &self.xfer_tx {
                    egui::SidePanel::left("file_browser")
                        .resizable(true)
                        .default_width(350.0)
                        .show(ctx, |ui| {
                            render_file_browser(ui, browser, xfer_tx);
                        });
                }
            }
        }

        // Main video panel
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(texture) = &self.video_texture {
                let size = texture.size_vec2();
                let rect = ui.available_rect_before_wrap();
                let img_size = size * (rect.width() / size.x).min(rect.height() / size.y);
                let pos = rect.min + (rect.size() - img_size) * 0.5;
                let image_rect = egui::Rect::from_min_size(pos, img_size);
                let img = egui::Image::new(egui::load::SizedTexture::new(texture.id(), image_rect.size()));
                ui.add(img);
                
                // Render OSD overlay on top of video
                if self.osd_visible {
                    self.render_osd_overlay(ui, &image_rect);
                }
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Connecting to remote...");
                });
            }
        });

        let input = ctx.input(|i| i.clone());
        if self.input_running {
            for event in &input.events {
                match event {
                    egui::Event::Key { key, pressed, .. } => {
                        let vk = key_to_vk(*key);
                        let flags = if *pressed { 0 } else { 0x0002 };
                        
                        // Handle OSD hotkeys
                        if *pressed {
                            match key {
                                egui::Key::F1 => {
                                    self.osd_visible = !self.osd_visible;
                                }
                                egui::Key::F2 => {
                                    self.toggle_input();
                                }
                                _ => {}
                            }
                        }
                    }
                    egui::Event::PointerButton { pos, button, pressed, .. } => {
                    }
                    egui::Event::PointerMoved(pos) => {
                    }
                    _ => {}
                }
            }
        }

        ctx.request_repaint();
    }
}

fn key_to_vk(key: egui::Key) -> u16 {
    match key {
        egui::Key::A => 0x41, egui::Key::B => 0x42, egui::Key::C => 0x43,
        egui::Key::D => 0x44, egui::Key::E => 0x45, egui::Key::F => 0x46,
        egui::Key::G => 0x47, egui::Key::H => 0x48, egui::Key::I => 0x49,
        egui::Key::J => 0x4A, egui::Key::K => 0x4B, egui::Key::L => 0x4C,
        egui::Key::M => 0x4D, egui::Key::N => 0x4E, egui::Key::O => 0x4F,
        egui::Key::P => 0x50, egui::Key::Q => 0x51, egui::Key::R => 0x52,
        egui::Key::S => 0x53, egui::Key::T => 0x54, egui::Key::U => 0x55,
        egui::Key::V => 0x56, egui::Key::W => 0x57, egui::Key::X => 0x58,
        egui::Key::Y => 0x59, egui::Key::Z => 0x5A,
        egui::Key::Num0 => 0x30, egui::Key::Num1 => 0x31, egui::Key::Num2 => 0x32,
        egui::Key::Num3 => 0x33, egui::Key::Num4 => 0x34, egui::Key::Num5 => 0x35,
        egui::Key::Num6 => 0x36, egui::Key::Num7 => 0x37, egui::Key::Num8 => 0x38,
        egui::Key::Num9 => 0x39,
        egui::Key::Enter => 0x0D, egui::Key::Escape => 0x1B, egui::Key::Tab => 0x09,
        egui::Key::Backspace => 0x08, egui::Key::Space => 0x20,
        egui::Key::ArrowLeft => 0x25, egui::Key::ArrowUp => 0x26,
        egui::Key::ArrowRight => 0x27, egui::Key::ArrowDown => 0x28,
        _ => 0,
    }
}