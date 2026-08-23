use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use egui::{Color32, Image, ImageSource, RichText, Sense, Stroke, TextStyle, Ui, Vec2};
use pyielink::client::DlCommand;
use walkdir::WalkDir;

/// SVG icons for file types (inline strings to avoid external files)
pub mod icons {
    pub const FOLDER_CLOSED: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"></path></svg>"#;
    
    pub const FOLDER_OPEN: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"></path></svg>"#;
    
    pub const FILE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"></path><polyline points="14 2 14 8 20 8"></polyline></svg>"#;
    
    pub const FILE_TEXT: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"></path><polyline points="14 2 14 8 20 8"></polyline><line x1="16" y1="13" x2="8" y2="13"></line><line x1="16" y1="17" x2="8" y2="17"></line><polyline points="10 9 9 9 8 9"></polyline></svg>"#;
    
    pub const FILE_IMAGE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="18" height="18" rx="2" ry="2"></rect><circle cx="8.5" cy="8.5" r="1.5"></circle><polyline points="21 15 16 10 5 21"></polyline></svg>"#;
    
    pub const ARROW_RIGHT: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="5" y1="12" x2="19" y2="12"></line><polyline points="12 5 19 12 12 19"></polyline></svg>"#;
    
    pub const ARROW_DOWN: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="6 9 12 15 18 9"></polyline></svg>"#;
    
    pub const UPLOAD: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"></path><polyline points="17 8 12 3 7 8"></polyline><line x1="12" y1="3" x2="12" y2="15"></line></svg>"#;
    
    pub const DOWNLOAD: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"></path><polyline points="7 10 12 15 17 10"></polyline><line x1="12" y1="15" x2="12" y2="3"></line></svg>"#;
    
    pub const REFRESH: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="23 4 23 10 17 10"></polyline><path d="M20.49 15a9 9 0 1 1-2.12-9.36L23 10"></path></svg>"#;
    
    pub const DELETE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6"></polyline><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"></path></svg>"#;
    
    pub const HOME: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 9l9-7 9 7v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"></path><polyline points="9 22 9 12 15 12 15 22"></polyline></svg>"#;
}

#[derive(Debug, Clone, PartialEq)]
pub enum FileType {
    Directory,
    File,
    Image,
    Text,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub name: String,
    pub file_type: FileType,
    pub size: u64,
    pub modified: Option<std::time::SystemTime>,
    pub is_expanded: bool,
    pub children: Vec<FileEntry>,
    pub is_loading: bool,
}

impl FileEntry {
    pub fn new(path: PathBuf) -> Self {
        let name = path.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        
        let (file_type, size) = if path.is_dir() {
            (FileType::Directory, 0)
        } else {
            let ext = path.extension()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let file_type = match ext.as_str() {
                "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" => FileType::Image,
                "txt" | "md" | "rs" | "toml" | "json" | "yaml" | "yml" | "log" | "cfg" | "ini" => FileType::Text,
                _ => FileType::File,
            };
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            (file_type, size)
        };
        
        Self {
            path,
            name,
            file_type,
            size,
            modified: None,
            is_expanded: false,
            children: Vec::new(),
            is_loading: false,
        }
    }
    
    pub fn icon(&self) -> &str {
        match self.file_type {
            FileType::Directory => {
                if self.is_expanded {
                    icons::FOLDER_OPEN
                } else {
                    icons::FOLDER_CLOSED
                }
            },
            FileType::Image => icons::FILE_IMAGE,
            FileType::Text => icons::FILE_TEXT,
            _ => icons::FILE,
        }
    }
    
    pub fn load_children(&mut self) -> anyhow::Result<()> {
        if !self.path.is_dir() {
            return Ok(());
        }
        
        self.children.clear();
        self.is_loading = true;
        
        for entry in WalkDir::new(&self.path).min_depth(1).max_depth(1).sort_by(|a, b| {
            let a_is_dir = a.file_type().is_dir();
            let b_is_dir = b.file_type().is_dir();
            match (a_is_dir, b_is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.file_name().cmp(b.file_name()),
            }
        }) {
            if let Ok(entry) = entry {
                let path = entry.path().to_path_buf();
                self.children.push(FileEntry::new(path));
            }
        }
        
        self.is_loading = false;
        Ok(())
    }
}

/// Remote file entry (from server listing)
#[derive(Debug, Clone)]
pub struct RemoteFileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub is_expanded: bool,
    pub children: Vec<RemoteFileEntry>,
    pub is_loading: bool,
}

impl RemoteFileEntry {
    pub fn new(name: String, path: String, is_dir: bool, size: u64) -> Self {
        Self {
            name,
            path,
            is_dir,
            size,
            is_expanded: false,
            children: Vec::new(),
            is_loading: false,
        }
    }
    
    pub fn file_type(&self) -> FileType {
        if self.is_dir {
            FileType::Directory
        } else {
            FileType::File
        }
    }
    
    pub fn icon(&self) -> &str {
        match self.file_type() {
            FileType::Directory => {
                if self.is_expanded { icons::FOLDER_OPEN } else { icons::FOLDER_CLOSED }
            },
            _ => icons::FILE,
        }
    }
}

/// File browser state
pub struct FileBrowser {
    pub local_root: FileEntry,
    pub remote_root: Option<RemoteFileEntry>,
    pub selected_local: Option<PathBuf>,
    pub selected_remote: Option<String>,
    pub local_path_history: Vec<PathBuf>,
    pub remote_path_history: Vec<String>,
    pub transfer_progress: Option<TransferProgress>,
    pub show_transfer_dialog: bool,
    pub drag_drop_active: bool,
}

#[derive(Debug, Clone)]
pub struct TransferProgress {
    pub filename: String,
    pub bytes_transferred: u64,
    pub total_bytes: u64,
    pub is_upload: bool,
    pub start_time: Instant,
}

impl FileBrowser {
    pub fn new(local_root_path: PathBuf) -> Self {
        let mut local_root = FileEntry::new(local_root_path.clone());
        let _ = local_root.load_children();
        
        Self {
            local_root,
            remote_root: None,
            selected_local: None,
            selected_remote: None,
            local_path_history: Vec::new(),
            remote_path_history: Vec::new(),
            transfer_progress: None,
            show_transfer_dialog: false,
            drag_drop_active: false,
        }
    }
    
    pub fn set_remote_root(&mut self, root: RemoteFileEntry) {
        self.remote_root = Some(root);
    }
    
    pub fn update_transfer_progress(&mut self, filename: String, bytes: u64, total: u64, is_upload: bool) {
        if self.transfer_progress.is_none() {
            self.transfer_progress = Some(TransferProgress {
                filename,
                bytes_transferred: bytes,
                total_bytes: total,
                is_upload,
                start_time: Instant::now(),
            });
        } else if let Some(progress) = &mut self.transfer_progress {
            progress.bytes_transferred = bytes;
            progress.total_bytes = total;
        }
        
        if bytes >= total {
            self.transfer_progress = None;
        }
    }
}

/// Render an SVG icon as an egui Image
fn svg_icon(ui: &mut Ui, svg: &str, size: f32, color: Color32) {
    // We'll use a label with the SVG as text for now
    // In a real implementation, you'd use egui's image support with a texture
    let _ = ui.label(RichText::new("📁").size(size)); // Placeholder
}

/// Render a file tree node
fn render_file_tree_node(ui: &mut Ui, entry: &mut FileEntry, selected: &Option<PathBuf>, on_select: &mut dyn FnMut(PathBuf), on_double_click: &mut dyn FnMut(PathBuf), depth: usize) -> bool {
    let is_selected = selected.as_ref() == Some(&entry.path);
    let indent = depth as f32 * 20.0;
    
    ui.horizontal(|ui| {
        ui.add_space(indent);
        
        // Expand/collapse arrow for directories
        if entry.file_type == FileType::Directory {
            let arrow = if entry.is_expanded { icons::ARROW_DOWN } else { icons::ARROW_RIGHT };
            if ui.add(egui::Button::new(RichText::new("▶").size(12.0)).frame(false).min_size(Vec2::new(16.0, 16.0))).clicked() {
                entry.is_expanded = !entry.is_expanded;
                if entry.is_expanded && entry.children.is_empty() && !entry.is_loading {
                    let _ = entry.load_children();
                }
            }
        } else {
            ui.add_space(16.0);
        }
        
        // Icon
        let icon_color = if entry.file_type == FileType::Directory {
            Color32::from_rgb(255, 200, 100)
        } else if entry.file_type == FileType::Image {
            Color32::from_rgb(100, 200, 255)
        } else {
            Color32::from_rgb(200, 200, 200)
        };
        
        // Use a simple text representation for now
        let icon_char = match entry.file_type {
            FileType::Directory => if entry.is_expanded { "📂" } else { "📁" },
            FileType::Image => "🖼️",
            FileType::Text => "📄",
            _ => "📄",
        };
        
        let response = ui.selectable_label(is_selected, format!("{} {}", icon_char, entry.name));
        
        if response.clicked() {
            on_select(entry.path.clone());
        }
        
        if response.double_clicked() {
            if entry.file_type == FileType::Directory {
                entry.is_expanded = !entry.is_expanded;
                if entry.is_expanded && entry.children.is_empty() && !entry.is_loading {
                    let _ = entry.load_children();
                }
            } else {
                on_double_click(entry.path.clone());
            }
        }
        
        // Context menu
        response.context_menu(|ui| {
            if ui.button("Download").clicked() {
                ui.close_menu();
            }
            if ui.button("Delete").clicked() {
                ui.close_menu();
            }
            if ui.button("Rename").clicked() {
                ui.close_menu();
            }
        });
        
        // Drag and drop
        if response.drag_started() {
            // Handle drag start
        }
    });
    
    if entry.is_expanded && entry.file_type == FileType::Directory {
        if entry.is_loading {
            ui.add_space(depth as f32 * 20.0 + 16.0);
            ui.spinner();
        } else {
            // Sort children: directories first, then files, alphabetically
            let mut sorted_children = entry.children.clone();
            sorted_children.sort_by(|a, b| {
                match (&a.file_type, &b.file_type) {
                    (FileType::Directory, FileType::Directory) => a.name.cmp(&b.name),
                    (FileType::Directory, _) => std::cmp::Ordering::Less,
                    (_, FileType::Directory) => std::cmp::Ordering::Greater,
                    _ => a.name.cmp(&b.name),
                }
            });
            
            for child in &mut sorted_children {
                if let Some(mut_entry) = entry.children.iter_mut().find(|c| c.path == child.path) {
                    render_file_tree_node(ui, mut_entry, selected, on_select, on_double_click, depth + 1);
                }
            }
        }
    }
    
    is_selected
}

/// Remote file tree node
fn render_remote_file_tree_node(ui: &mut Ui, entry: &mut RemoteFileEntry, selected: &Option<String>, on_select: &mut dyn FnMut(String), on_double_click: &mut dyn FnMut(String), depth: usize) {
    let is_selected = selected.as_ref() == Some(&entry.path);
    let indent = depth as f32 * 20.0;
    
    ui.horizontal(|ui| {
        ui.add_space(indent);
        
        if entry.is_dir {
            let arrow = if entry.is_expanded { icons::ARROW_DOWN } else { icons::ARROW_RIGHT };
            if ui.add(egui::Button::new("▼").frame(false).min_size(Vec2::new(16.0, 16.0))).clicked() {
                entry.is_expanded = !entry.is_expanded;
            }
        } else {
            ui.add_space(16.0);
        }
        
        let response = ui.selectable_label(is_selected, format!("{} {}", 
            if entry.is_dir { "📁" } else { "📄" }, 
            entry.name));
        
        if response.clicked() {
            on_select(entry.path.clone());
        }
        
        if response.double_clicked() {
            if entry.is_dir {
                entry.is_expanded = !entry.is_expanded;
            } else {
                on_double_click(entry.path.clone());
            }
        }
    });
    
    if entry.is_expanded && entry.is_dir {
        if entry.is_loading {
            ui.add_space(depth as f32 * 20.0 + 16.0);
            ui.spinner();
        } else {
            for child in &mut entry.children {
                render_remote_file_tree_node(ui, child, selected, on_select, on_double_click, depth + 1);
            }
        }
    }
}

/// Transfer progress bar
pub fn render_transfer_progress(ui: &mut Ui, progress: &TransferProgress) {
    let percent = if progress.total_bytes > 0 {
        progress.bytes_transferred as f32 / progress.total_bytes as f32
    } else {
        0.0
    };
    
    let elapsed = progress.start_time.elapsed().as_secs_f32();
    let speed = if elapsed > 0.0 {
        progress.bytes_transferred as f32 / elapsed / 1024.0
    } else {
        0.0
    };
    
    let eta = if speed > 0.0 && progress.total_bytes > progress.bytes_transferred {
        (progress.total_bytes - progress.bytes_transferred) as f32 / (speed * 1024.0)
    } else {
        0.0
    };
    
    ui.group(|ui| {
        ui.horizontal(|ui| {
            let icon = if progress.is_upload { icons::UPLOAD } else { icons::DOWNLOAD };
            ui.label("⬆");
            ui.vertical(|ui| {
                ui.label(RichText::new(&progress.filename).strong());
                ui.add(egui::ProgressBar::new(percent).show_percentage());
                ui.label(format!("{:.1} KB/s  •  ETA: {:.0}s", speed, eta));
            });
        });
        
        if ui.button("Cancel").clicked() {
            // Handle cancel
        }
    });
}

/// Main file browser panel
pub fn render_file_browser(ui: &mut Ui, browser: &mut FileBrowser, xfer_tx: &std::sync::mpsc::Sender<DlCommand>) {
    ui.columns(2, |cols| {
        // Local file tree (left)
        cols[0].group(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Local").strong().size(16.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⟳").clicked() {
                        browser.local_root.load_children().ok();
                    }
                    if ui.button("🏠").clicked() {
                        // Go to home
                    }
                });
            });
            
            ui.separator();
            
            egui::ScrollArea::vertical().show(ui, |ui| {
                let mut selected = browser.selected_local.clone();
                let mut on_select = |path: PathBuf| {
                    browser.selected_local = Some(path);
                };
                let mut on_double_click = |path: PathBuf| {
                    // Could trigger download/upload
                };
                
                render_file_tree_node(ui, &mut browser.local_root, &mut selected, &mut on_select, &mut on_double_click, 0);
                browser.selected_local = selected;
            });
        });
        
        // Remote file tree (right)
        cols[1].group(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Remote").strong().size(16.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⟳").clicked() {
                        // Request remote file list
                    }
                });
            });
            
            ui.separator();
            
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some(remote_root) = &mut browser.remote_root {
                    let mut selected = browser.selected_remote.clone();
                    let mut on_select = |path: String| {
                        browser.selected_remote = Some(path);
                    };
                    let mut on_double_click = |path: String| {
                        // Handle double click on remote file
                    };
                    
                    render_remote_file_tree_node(ui, remote_root, &mut selected, &mut on_select, &mut on_double_click, 0);
                    browser.selected_remote = selected;
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label("Not connected to remote");
                    });
                }
            });
        });
    });
    
    // Transfer progress at bottom
    if let Some(progress) = &browser.transfer_progress {
        ui.add_space(10.0);
        render_transfer_progress(ui, progress);
    }
    
    // Handle drag and drop for uploads (egui 0.29 API)
    // Note: egui 0.29 uses a different API for dropped files
    // This will be implemented when the API is confirmed
}