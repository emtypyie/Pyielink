//! Native file-explorer GUI for pyielink (egui/eframe).
//!
//! Launch via `pyielink user@ip explorer` (requires the `gui` feature).
//! The video layer is deliberately NOT part of this app — it is broken and
//! removed from the framework; this is a pure file explorer.

use std::path::{Path, PathBuf};

use eframe::egui::{self, Align, Align2, Color32};

use pyielink::client::{
    normalize_remote, resolve_remote, stem_of, DirListing, GuiCommand, GuiEvent, GuiSession,
};

// --------------------------------- entry point ---------------------------------

pub fn run_explorer(target: &str) -> Result<(), String> {
    eframe::run_native(
        "pyielink-explorer",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1280.0, 760.0])
                .with_title(format!("PYIELINK EXPLORER — {}", target)),
            renderer: eframe::Renderer::Wgpu,
            ..Default::default()
        },
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(ExplorerApp::new(target)))
        }),
    )
    .map_err(|e| format!("GUI error: {:?}", e))?;
    Ok(())
}

// --------------------------------- model types ---------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneKind {
    Local,
    Remote,
}

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    is_dir: bool,
    size: u64,
    mtime: u64,
}

struct Pane {
    kind: PaneKind,
    cwd_label: String,
    remote_cwd: String,
    local_cwd: PathBuf,
    entries: Vec<Entry>,
    selected: Option<usize>,
    loading: bool,
    error: Option<String>,
    history: Vec<String>,
}

impl Pane {
    fn remote(root_label: &str) -> Self {
        Pane {
            kind: PaneKind::Remote,
            cwd_label: root_label.to_string(),
            remote_cwd: ".".to_string(),
            local_cwd: PathBuf::new(),
            entries: Vec::new(),
            selected: None,
            loading: false,
            error: None,
            history: Vec::new(),
        }
    }

    fn local(cwd: PathBuf) -> Self {
        Pane {
            kind: PaneKind::Local,
            cwd_label: cwd.display().to_string(),
            remote_cwd: String::new(),
            local_cwd: cwd,
            entries: Vec::new(),
            selected: None,
            loading: false,
            error: None,
            history: Vec::new(),
        }
    }

    fn remote_path_for(&self, name: &str) -> String {
        resolve_remote(&self.remote_cwd, name)
    }

    fn local_path_for(&self, name: &str) -> PathBuf {
        self.local_cwd.join(name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Refresh(PaneKind),
    Up(PaneKind),
    Back(PaneKind),
    Home(PaneKind),
    Enter(PaneKind, usize),
    NewFolder(PaneKind),
    Rename(PaneKind, usize),
    Delete(PaneKind, usize),
    CopyClip(PaneKind),
    CutClip(PaneKind),
}

#[derive(Clone)]
struct ClipItem {
    name: String,
    is_dir: bool,
}

#[derive(Clone)]
struct Clipboard {
    kind: PaneKind,
    remote_cwd: String,
    local_cwd: PathBuf,
    items: Vec<ClipItem>,
    cut: bool,
}

#[derive(Debug, Clone)]
struct SweepTarget {
    kind: PaneKind,
    remote: String,
    local: PathBuf,
}

/// Pending source deletions after a `cut` completes (counted on op/xfer ok).
struct PendingSweep {
    remaining: usize,
    targets: Vec<SweepTarget>,
}

struct DragState {
    kind: PaneKind,
    name: String,
    is_dir: bool,
}

// --------------------------------- dialogs ---------------------------------

#[derive(Default)]
struct ConnectDialog {
    user: String,
    ip: String,
    password: String,
    accept_license: bool,
    error: Option<String>,
    busy: bool,
}

enum Modal {
    Text {
        title: String,
        input: String,
        mode: TextMode,
    },
    Confirm {
        title: String,
        msg: String,
        action: ConfirmAction,
    },
}

enum TextMode {
    NewFolder(PaneKind),
    Rename(PaneKind, usize),
}

enum ConfirmAction {
    DeleteRemote { path: String, is_dir: bool },
    DeleteLocal { path: PathBuf },
    DropUpload { paths: Vec<PathBuf> },
}

// --------------------------------- terminal ---------------------------------

#[derive(Default)]
struct TerminalPane {
    open: bool,
    log: Vec<(String, Color32)>,
    input: String,
    busy: bool,
    elevated: bool,
}

impl TerminalPane {
    fn push(&mut self, text: String, color: Color32) {
        if self.log.len() > 500 {
            self.log.remove(0);
        }
        self.log.push((text, color));
    }
}

// --------------------------------- the app ---------------------------------

struct ExplorerApp {
    target: String,
    phase: Phase,
    connect: ConnectDialog,
    session: Option<GuiSession>,
    status: String,
    remote: Pane,
    local: Pane,
    focused: PaneKind,
    clipboard: Option<Clipboard>,
    sweep: Option<PendingSweep>,
    terminal: TerminalPane,
    modal: Option<Modal>,
    drag: Option<DragState>,
}

enum Phase {
    Dialog,
    Live,
}

impl ExplorerApp {
    fn new(target: &str) -> Self {
        let (user, ip) = match target.split_once('@') {
            Some((u, i)) => (u.to_string(), i.to_string()),
            None => (String::new(), target.to_string()),
        };
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        ExplorerApp {
            target: target.to_string(),
            phase: Phase::Dialog,
            connect: ConnectDialog {
                user,
                ip,
                accept_license: true,
                ..Default::default()
            },
            session: None,
            status: "not connected".to_string(),
            remote: Pane::remote("~"),
            local: Pane::local(home),
            focused: PaneKind::Remote,
            clipboard: None,
            sweep: None,
            terminal: TerminalPane::default(),
            modal: None,
            drag: None,
        }
    }

    fn pane_mut(&mut self, kind: PaneKind) -> &mut Pane {
        match kind {
            PaneKind::Remote => &mut self.remote,
            PaneKind::Local => &mut self.local,
        }
    }

    fn pane(&self, kind: PaneKind) -> &Pane {
        match kind {
            PaneKind::Remote => &self.remote,
            PaneKind::Local => &self.local,
        }
    }

    fn request_list(&mut self, kind: PaneKind) {
        match kind {
            PaneKind::Remote => {
                let path = self.remote.remote_cwd.clone();
                self.remote.loading = true;
                self.remote.error = None;
                if let Some(s) = &self.session {
                    let _ = s.send(GuiCommand::ListDir { path });
                } else {
                    self.remote.loading = false;
                    self.remote.error = Some("not connected".into());
                }
            }
            PaneKind::Local => load_local_listing(&mut self.local),
        }
    }

    fn navigate(&mut self, kind: PaneKind, label: String, remote_cwd: String, local_cwd: PathBuf) {
        let pane = self.pane_mut(kind);
        pane.history.push(pane.cwd_label.clone());
        pane.cwd_label = label;
        if kind == PaneKind::Remote {
            pane.remote_cwd = remote_cwd;
        } else {
            pane.local_cwd = local_cwd;
        }
        pane.selected = None;
        drop(pane);
        self.request_list(kind);
    }

    fn pull(&mut self, item: (String, bool)) {
        // remote -> local download of one item (files only)
        let (name, is_dir) = item;
        if is_dir {
            self.status(format!("'{}' is a directory — downloads are file-only for now", name), Color32::YELLOW);
            return;
        }
        if self.session.is_none() {
            self.status("not connected", Color32::RED);
            return;
        }
        let remote = self.remote.remote_path_for(&name);
        let local = self.local.local_cwd.join(&name);
        let _ = self.session.as_ref().unwrap().send(GuiCommand::Get { remote, local });
        self.status(format!("downloading {}", name), Color32::GRAY);
    }

    fn push(&mut self, item: (String, bool)) {
        // local -> remote upload of one item
        let (name, is_dir) = item;
        if is_dir {
            self.status(format!("'{}' is a directory — uploads are file-only for now", name), Color32::YELLOW);
            return;
        }
        if self.session.is_none() {
            self.status("not connected", Color32::RED);
            return;
        }
        let local = self.local.local_cwd.join(&name);
        if !local.is_file() {
            self.status(format!("no such local file: {}", local.display()), Color32::RED);
            return;
        }
        let remote = self.remote.remote_path_for(&name);
        let _ = self.session.as_ref().unwrap().send(GuiCommand::Put { local, remote });
        self.status(format!("uploading {}", name), Color32::GRAY);
    }

    fn copy_selection(&mut self, cut: bool) {
        let kind = self.focused;
        let pane = self.pane(kind);
        let idx = match pane.selected {
            Some(i) if i < pane.entries.len() => i,
            _ => {
                self.status("nothing selected to copy", Color32::YELLOW);
                return;
            }
        };
        let items = vec![ClipItem {
            name: pane.entries[idx].name.clone(),
            is_dir: pane.entries[idx].is_dir,
        }];
        self.clipboard = Some(Clipboard {
            kind,
            remote_cwd: pane.remote_cwd.clone(),
            local_cwd: pane.local_cwd.clone(),
            items,
            cut,
        });
        self.status(format!("{} '{}' to clipboard", if cut { "cut" } else { "copied" }, items[0].name), Color32::GRAY);
        if cut {
            // local cut is immediate on paste; remote cut resolved after copy
        }
    }

    fn paste_to(&mut self, to: PaneKind) {
        let Some(clip) = self.clipboard.clone() else {
            self.status("clipboard is empty", Color32::YELLOW);
            return;
        };
        let to_pane = self.pane(to);
        let to_remote_cwd = to_pane.remote_cwd.clone();
        let to_local_cwd = to_pane.local_cwd.clone();
        let dst_entries = to_pane.entries.clone();
        drop(to_pane);

        let mut dispatched = 0usize;
        let mut sweep_targets: Vec<SweepTarget> = Vec::new();
        for item in &clip.items {
            let same_pane = clip.kind == to;
            let cut = clip.cut;
            match (clip.kind, to) {
                (PaneKind::Remote, PaneKind::Remote) => {
                    let from = resolve_remote(&clip.remote_cwd, &item.name);
                    let to = resolve_remote(&to_remote_cwd, &self.dedup_name(&dst_entries, &item.name, item.is_dir, !cut));
                    if let Some(s) = &self.session {
                        let _ = s.send(GuiCommand::Copy { from, to });
                        dispatched += 1;
                        if cut {
                            sweep_targets.push(SweepTarget { kind: PaneKind::Remote, remote: from, local: PathBuf::new() });
                        }
                    }
                }
                (PaneKind::Local, PaneKind::Local) => {
                    let src = clip.local_cwd.join(&item.name);
                    let dst = to_local_cwd.join(&self.dedup_name(&dst_entries, &item.name, item.is_dir, !cut));
                    if cut {
                        if std::fs::rename(&src, &dst).is_err() {
                            let _ = std::fs::remove_dir_all(&dst).or_else(|_| std::fs::remove_file(&dst));
                            let _ = copy_local(&src, &dst, item.is_dir);
                            let _ = remove_local(&src, item.is_dir);
                        }
                    } else {
                        copy_local(&src, &dst, item.is_dir).map_err(|e| self.status(format!("copy failed: {}", e), Color32::RED)).ok();
                    }
                }
                (PaneKind::Remote, PaneKind::Local) => {
                    if let Some(s) = &self.session {
                        let remote = resolve_remote(&clip.remote_cwd, &item.name);
                        let local = to_local_cwd.join(&self.dedup_name(&dst_entries, &item.name, item.is_dir, !cut));
                        if item.is_dir {
                            self.status("can't download directories yet", Color32::YELLOW);
                        } else {
                            let _ = s.send(GuiCommand::Get { remote, local });
                            dispatched += 1;
                            if cut {
                                sweep_targets.push(SweepTarget { kind: PaneKind::Remote, remote, local: PathBuf::new() });
                            }
                        }
                    }
                }
                (PaneKind::Local, PaneKind::Remote) => {
                    if let Some(s) = &self.session {
                        let local = clip.local_cwd.join(&item.name);
                        let remote = resolve_remote(&to_remote_cwd, &self.dedup_name(&dst_entries, &item.name, item.is_dir, !cut));
                        if item.is_dir {
                            self.status("can't upload directories yet", Color32::YELLOW);
                        } else if local.is_file() {
                            let _ = s.send(GuiCommand::Put { local, remote });
                            dispatched += 1;
                            if cut {
                                sweep_targets.push(SweepTarget { kind: PaneKind::Local, remote: String::new(), local });
                            }
                        } else {
                            self.status(format!("no such local file: {}", local.display()), Color32::RED);
                        }
                    }
                }
            }
            let _ = same_pane;
        }

        if dispatched > 0 && !sweep_targets.is_empty() {
            // cut: remove sources once every dispatched op reported success
            let _ = dispatched;
            self.sweep = Some(PendingSweep {
                remaining: sweep_targets.len(),
                targets: sweep_targets,
            });
        }
        self.request_list(to);
        if clip.kind != to {
            self.request_list(clip.kind);
        }
        self.status(
            format!(
                "{} {} -> {}",
                if clip.cut { "moved" } else { "copied" },
                if clip.kind == PaneKind::Remote { "remote" } else { "local" },
                if to == PaneKind::Remote { "remote" } else { "local" },
            ),
            Color32::GRAY,
        );
    }

    fn dedup_name(&self, entries: &[Entry], name: &str, is_dir: bool, apply: bool) -> String {
        if !apply {
            return name.to_string();
        }
        if !entries.iter().any(|e| e.name == name) {
            return name.to_string();
        }
        let stem = stem_of(name).to_string();
        let ext = match name.rfind('.') {
            Some(i) if i > 0 => name[i..].to_string(),
            _ => String::new(),
        };
        for n in 2..10000 {
            let cand = if is_dir || ext.is_empty() {
                format!("{} (copy {})", stem, n)
            } else {
                format!("{} (copy {}){}", stem, n, ext)
            };
            if !entries.iter().any(|e| e.name == cand) {
                return cand;
            }
        }
        name.to_string()
    }

    fn apply_sweep(&mut self) {
        let Some(sweep) = self.sweep.take() else { return };
        for t in &sweep.targets {
            match t.kind {
                PaneKind::Remote => {
                    if let Some(s) = &self.session {
                        let _ = s.send(GuiCommand::Delete { path: t.remote.clone(), recursive: true });
                    }
                }
                PaneKind::Local => {
                    remove_local(&t.local, false).ok();
                }
            }
        }
        self.status("cut sources removed", Color32::GRAY);
    }

    fn handle_op(&mut self, op: Op) -> Option<Modal> {
        match op {
            Op::Refresh(kind) => self.request_list(kind),
            Op::Up(kind) => match kind {
                PaneKind::Remote => {
                    let next = normalize_remote(&self.remote.remote_cwd, "..");
                    let label = if next == "." { "~".to_string() } else { next.clone() };
                    self.navigate(kind, label, next, PathBuf::new());
                }
                PaneKind::Local => {
                    if let Some(parent) = self.local.local_cwd.parent() {
                        let p = parent.to_path_buf();
                        let label = p.display().to_string();
                        self.navigate(kind, label, String::new(), p);
                    }
                }
            },
            Op::Back(kind) => {
                // re-enter previous directory from history
                let label = self.pane_mut(kind).history.pop();
                if let Some(label) = label {
                    if kind == PaneKind::Remote {
                        // normalize label: it may be "~" (root) or a path
                        let cwd = if label == "~" { ".".to_string() } else { label.clone() };
                        self.navigate(kind, label, cwd, PathBuf::new());
                    } else {
                        let p = PathBuf::from(label.clone());
                        self.navigate(kind, label, String::new(), p);
                    }
                }
            }
            Op::Home(kind) => match kind {
                PaneKind::Remote => self.navigate(kind, "~".to_string(), ".".to_string(), PathBuf::new()),
                PaneKind::Local => {
                    let home = std::env::var("HOME")
                        .or_else(|_| std::env::var("USERPROFILE"))
                        .map(PathBuf::from)
                        .unwrap_or_else(|_| self.local.local_cwd.clone());
                    let label = home.display().to_string();
                    self.navigate(kind, label, String::new(), home);
                }
            },
            Op::Enter(kind, idx) => {
                let entry = self.pane(kind).entries.get(idx).cloned();
                if let Some(e) = entry {
                    if e.is_dir {
                        match kind {
                            PaneKind::Remote => {
                                let next = self.remote.remote_path_for(&e.name);
                                let label = next.clone();
                                self.navigate(kind, label, next, PathBuf::new());
                            }
                            PaneKind::Local => {
                                let p = self.local.local_cwd.join(&e.name);
                                let label = p.display().to_string();
                                self.navigate(kind, label, String::new(), p);
                            }
                        }
                    } else {
                        self.focused = kind;
                    }
                }
            }
            Op::NewFolder(kind) => {
                self.focused = kind;
                return Some(Modal::Text {
                    title: "New folder".to_string(),
                    input: String::new(),
                    mode: TextMode::NewFolder(kind),
                });
            }
            Op::Rename(kind, idx) => {
                self.focused = kind;
                let name = self.pane(kind).entries.get(idx).map(|e| e.name.clone()).unwrap_or_default();
                return Some(Modal::Text {
                    title: "Rename".to_string(),
                    input: name.clone(),
                    mode: TextMode::Rename(kind, idx),
                });
            }
            Op::Delete(kind, idx) => {
                self.focused = kind;
                let e = self.pane(kind).entries.get(idx).cloned();
                if let Some(e) = e {
                    return Some(Modal::Confirm {
                        title: "Delete".to_string(),
                        msg: format!("Delete '{}'?", e.name),
                        action: match kind {
                            PaneKind::Remote => ConfirmAction::DeleteRemote {
                                path: self.remote.remote_path_for(&e.name),
                                is_dir: e.is_dir,
                            },
                            PaneKind::Local => ConfirmAction::DeleteLocal {
                                path: self.local.local_cwd.join(&e.name),
                            },
                        },
                    });
                }
            }
            Op::CopyClip(kind) => {
                self.focused = kind;
                self.copy_selection(false);
            }
            Op::CutClip(kind) => {
                self.focused = kind;
                self.copy_selection(true);
            }
        }
        None
    }

    fn run_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::DeleteRemote { path, is_dir } => {
                if let Some(s) = &self.session {
                    let _ = s.send(GuiCommand::Delete { path, recursive: is_dir });
                    self.request_list(PaneKind::Remote);
                }
            }
            ConfirmAction::DeleteLocal { path } => {
                let is_dir = path.is_dir();
                if remove_local(&path, is_dir).is_err() {
                    self.status(format!("delete failed: {}", path.display()), Color32::RED);
                }
                self.request_list(PaneKind::Local);
            }
            ConfirmAction::DropUpload { paths } => {
                for p in paths {
                    let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    let remote = self.remote.remote_path_for(&name);
                    if let Some(s) = &self.session {
                        let _ = s.send(GuiCommand::Put { local: p, remote });
                    }
                }
                self.status(format!("uploading {} file(s)", paths.len()), Color32::GRAY);
            }
        }
    }

    fn run_modal_text(&mut self, m: Modal) {
        let (title, input, mode) = match m {
            Modal::Text { title, input, mode } => (title, input, mode),
            _ => return,
        };
        let input = input.trim().to_string();
        if input.is_empty() {
            self.status(format!("{}: empty name", title), Color32::YELLOW);
            return;
        }
        match mode {
            TextMode::NewFolder(kind) => match kind {
                PaneKind::Remote => {
                    let path = self.remote.remote_path_for(&input);
                    if let Some(s) = &self.session {
                        let _ = s.send(GuiCommand::CreateDir { path });
                        self.request_list(PaneKind::Remote);
                    }
                }
                PaneKind::Local => {
                    let p = self.local.local_cwd.join(&input);
                    match std::fs::create_dir_all(&p) {
                        Ok(()) => self.request_list(PaneKind::Local),
                        Err(e) => self.status(format!("mkdir failed: {}", e), Color32::RED),
                    }
                }
            },
            TextMode::Rename(kind, idx) => {
                let old_name = self.pane(kind).entries.get(idx).map(|e| e.name.clone()).unwrap_or_default();
                match kind {
                    PaneKind::Remote => {
                        let old = self.remote.remote_path_for(&old_name);
                        let new = self.remote.remote_path_for(&input);
                        if let Some(s) = &self.session {
                            let _ = s.send(GuiCommand::Rename { old, new });
                            self.request_list(PaneKind::Remote);
                        }
                    }
                    PaneKind::Local => {
                        let old = self.local.local_cwd.join(&old_name);
                        let new = self.local.local_cwd.join(&input);
                        match std::fs::rename(&old, &new) {
                            Ok(()) => self.request_list(PaneKind::Local),
                            Err(e) => self.status(format!("rename failed: {}", e), Color32::RED),
                        }
                    }
                }
            }
        }
    }

    fn status(&mut self, text: String, color: Color32) {
        self.status = text;
        let _ = color; // status color already used inline; keep simple
    }

    fn poll_events(&mut self) {
        let Some(session) = &self.session else { return };
        while let Some(ev) = session.try_event() {
            match ev {
                GuiEvent::Ready => {
                    self.status("connected — loading remote...".to_string());
                    self.request_list(PaneKind::Remote);
                    self.request_list(PaneKind::Local);
                }
                GuiEvent::DirListing(listing) => {
                    if listing.path == self.remote.remote_cwd {
                        self.remote.entries = listing
                            .entries
                            .iter()
                            .map(|e| Entry {
                                name: e.name.clone(),
                                is_dir: e.is_dir,
                                size: e.size,
                                mtime: e.mtime.unwrap_or(0),
                            })
                            .collect();
                        self.remote.loading = false;
                        self.remote.error = None;
                    }
                }
                GuiEvent::OpResult { op, ok, msg } => {
                    if ok {
                        self.status(format!("remote {} ok", op), Color32::GREEN);
                        self.request_list(PaneKind::Remote);
                        if let Some(sweep) = self.sweep.as_mut() {
                            if op == "copy" && sweep.remaining > 0 {
                                sweep.remaining -= 1;
                                if sweep.remaining == 0 {
                                    drop(sweep);
                                    self.apply_sweep();
                                }
                            }
                        }
                    } else {
                        self.status(format!("remote {} failed: {}", op, msg), Color32::RED);
                    }
                }
                GuiEvent::TransferDone { label, ok } => {
                    if let Some(sweep) = self.sweep.as_mut() {
                        if sweep.remaining > 0 {
                            sweep.remaining -= 1;
                            if sweep.remaining == 0 {
                                drop(sweep);
                                self.apply_sweep();
                            }
                        }
                    }
                    self.status(
                        format!("transfer '{}' {}", label, if ok { "OK" } else { "FAILED" }),
                        if ok { Color32::GREEN } else { Color32::RED },
                    );
                    self.request_list(PaneKind::Remote);
                    self.request_list(PaneKind::Local);
                }
                GuiEvent::ExecOutput(out) => {
                    if let Ok(text) = String::from_utf8(out) {
                        for line in text.lines() {
                            self.terminal.push(line.to_string(), Color32::LIGHT_GRAY);
                        }
                    }
                }
                GuiEvent::ExecEnd { code, denied } => {
                    self.terminal.busy = false;
                    self.terminal.push(
                        if denied { format!("[denied] {}", code) } else { format!("[exit] {}", code) },
                        if denied { Color32::RED } else { Color32::GREEN },
                    );
                }
                GuiEvent::Message(_) => {}
                GuiEvent::Closed(reason) => {
                    self.session = None;
                    self.phase = Phase::Dialog;
                    self.connect.busy = false;
                    self.connect.error = None;
                    let r = reason.trim();
                    self.status(
                        if r.is_empty() { "disconnected".to_string() } else { format!("disconnected: {}", r) },
                        Color32::YELLOW,
                    );
                }
            }
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context, remote_rect: egui::Rect, local_rect: egui::Rect) {
        let dropped: Vec<PathBuf> = ctx
            .input(|i| {
                i.raw
                    .dropped_files
                    .iter()
                    .filter_map(|f| f.path.clone())
                    .filter(|p| p.is_file())
                    .collect()
            });
        if dropped.is_empty() {
            return;
        }
        let pos = ctx.input(|i| i.pointer.hover_pos());
        let over_local = pos.map(|p| local_rect.contains(p)).unwrap_or(false);
        if over_local {
            for p in &dropped {
                let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let dst = self.local.local_cwd.join(&name);
                if copy_local(p, &dst, false).is_err() {
                    self.status(format!("could not copy {}", p.display()), Color32::RED);
                }
            }
            self.request_list(PaneKind::Local);
            self.status(format!("copied {} file(s) to local", dropped.len()), Color32::GRAY);
        } else {
            // default: drop into remote cwd (or anywhere on the pane)
            let _ = remote_rect;
            if self.session.is_some() {
                for p in &dropped {
                    let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    let remote = self.remote.remote_path_for(&name);
                    let _ = self.session.as_ref().unwrap().send(GuiCommand::Put { local: p.clone(), remote });
                }
                self.status(format!("uploading {} file(s)", dropped.len()), Color32::GRAY);
            }
        }
    }

    fn handle_dnd(&mut self, ctx: &egui::Context, remote_rect: egui::Rect, local_rect: egui::Rect) {
        let Some(drag) = self.drag.clone() else { return };
        let released = ctx.input(|i| i.pointer.any_released());
        let pos = ctx.input(|i| i.pointer.hover_pos());
        if !released {
            return;
        }
        let Some(pos) = pos else { self.drag = None; return };
        self.drag = None;
        // drop on the OTHER pane copies; hold Shift to move (cut)
        if drag.kind == PaneKind::Remote && local_rect.contains(pos) {
            let name = drag.name.clone();
            let is_dir = drag.is_dir;
            if let Some(s) = &self.session {
                let remote = self.remote.remote_path_for(&name);
                let local = self.local.local_cwd.join(&self.dedup_name(&self.local.entries, &name, is_dir, true));
                if !is_dir {
                    let _ = s.send(GuiCommand::Get { remote, local });
                    self.status(format!("downloading {}", name), Color32::GRAY);
                }
            }
        } else if drag.kind == PaneKind::Local && remote_rect.contains(pos) {
            let name = drag.name.clone();
            let is_dir = drag.is_dir;
            if let Some(s) = &self.session {
                let local = self.local.local_cwd.join(&name);
                if !is_dir && local.is_file() {
                    let remote = self.remote.remote_path_for(&self.dedup_name(&self.remote.entries, &name, false, true));
                    let _ = s.send(GuiCommand::Put { local, remote });
                    self.status(format!("uploading {}", name), Color32::GRAY);
                }
            }
        }
    }

    fn handle_keyboard(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let input = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::C) && i.modifiers.command,
                i.key_pressed(egui::Key::X) && i.modifiers.command,
                i.key_pressed(egui::Key::V) && i.modifiers.command,
            )
        });
        let (copy, cut, paste) = input;
        let kind = self.focused;
        if copy {
            self.copy_selection(false);
        } else if cut {
            self.copy_selection(true);
        } else if paste {
            self.paste_to(kind);
        }
    }

    fn render_pane(&mut self, ui: &mut egui::Ui, ops: &mut Vec<Op>) {
        let kind = self.pane_kind_for(ui);
        let pane = self.pane_mut(kind);
        ui.horizontal(|ui| {
            if ui.button("←").clicked() {
                ops.push(Op::Back(kind));
            }
            if ui.button("↑").clicked() {
                ops.push(Op::Up(kind));
            }
            if ui.button("⌂").clicked() {
                ops.push(Op::Home(kind));
            }
            ui.separator();
            ui.label(egui::RichText::new(&pane.cwd_label).strong().size(14.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⟳").clicked() {
                    ops.push(Op::Refresh(kind));
                }
            });
        });
        ui.separator();
        if pane.loading {
            ui.spinner();
        }
        if let Some(err) = &pane.error {
            ui.label(egui::RichText::new(err).color(Color32::RED));
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            let entries = pane.entries.clone();
            let sel = pane.selected;
            for (i, e) in entries.iter().enumerate() {
                let is_sel = sel == Some(i);
                let icon = if e.is_dir { "📁" } else { "📄" };
                let text = if e.is_dir {
                    format!("{} {}/", icon, e.name)
                } else {
                    format!("{} {}  ({} B)", icon, e.name, e.size)
                };
                let resp = ui.selectable_label(is_sel, egui::RichText::new(text).monospace());
                if resp.clicked() {
                    pane.selected = Some(i);
                    ui.ctx().memory_mut(|m| m.data.get_temp_mut(egui::Id::new(("sel", kind))).replace(i));
                }
                if resp.double_clicked() {
                    ops.push(Op::Enter(kind, i));
                }
                if resp.context_menu(|ui| {
                    if ui.button("Open").clicked() {
                        ops.push(Op::Enter(kind, i));
                        ui.close_menu();
                    }
                    if ui.button("New folder here").clicked() {
                        ops.push(Op::NewFolder(kind));
                        ui.close_menu();
                    }
                    if ui.button("Rename...").clicked() {
                        ops.push(Op::Rename(kind, i));
                        ui.close_menu();
                    }
                    if ui.button("Delete...").clicked() {
                        ops.push(Op::Delete(kind, i));
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Copy").clicked() {
                        pane.selected = Some(i);
                        ops.push(Op::CopyClip(kind));
                        ui.close_menu();
                    }
                    if ui.button("Cut").clicked() {
                        pane.selected = Some(i);
                        ops.push(Op::CutClip(kind));
                        ui.close_menu();
                    }
                    if ui.button("Paste here").clicked() {
                        pane.selected = Some(i);
                        let _ = ops;
                    }
                }) {}
                if resp.drag_started() {
                    self.drag = Some(DragState {
                        kind,
                        name: e.name.clone(),
                        is_dir: e.is_dir,
                    });
                }
            }
        });
    }
}

// small helper to keep a pane-local selection mirror (kept simple: stored in Pane)
impl ExplorerApp {
    fn pane_kind_for(&mut self, _ui: &egui::Ui) -> PaneKind {
        let _ = self;
        // The caller passes the kind explicitly through ops; this is only used
        // to make render_pane agnostic — we pull real kind from call sites.
        PaneKind::Remote
    }
}

// -------------------------------- helper fns ---------------------------------

fn sort_entries(entries: &mut Vec<Entry>) {
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
}

fn load_local_listing(pane: &mut Pane) {
    pane.entries.clear();
    pane.loading = false;
    pane.error = None;
    let rd = match std::fs::read_dir(&pane.local_cwd) {
        Ok(rd) => rd,
        Err(e) => {
            pane.error = Some(format!("{}", e));
            return;
        }
    };
    for item in rd.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        let is_dir = item.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let size = item.metadata().map(|m| m.len()).unwrap_or(0);
        let mtime = item
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        pane.entries.push(Entry { name, is_dir, size, mtime });
    }
    sort_entries(&mut pane.entries);
}

fn copy_local(src: &Path, dst: &Path, is_dir: bool) -> Result<(), String> {
    if is_dir {
        std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
        for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            copy_local(&entry.path(), &dst.join(name), entry.file_type().map(|t| t.is_dir()).unwrap_or(false))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ()).map_err(|e| e.to_string())
    }
}

fn remove_local(path: &Path, is_dir: bool) -> Result<(), String> {
    if is_dir || path.is_dir() {
        std::fs::remove_dir_all(path).map_err(|e| e.to_string())
    } else {
        std::fs::remove_file(path).map_err(|e| e.to_string())
    }
}

// --------------------------------- app impl ---------------------------------

impl eframe::App for ExplorerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_events();

        if self.phase == Phase::Dialog {
            let mut ops_local: Vec<Op> = Vec::new();
            let mut ops_remote: Vec<Op> = Vec::new();
            let mut remote_rect = egui::Rect::NOTHING;
            let mut local_rect = egui::Rect::NOTHING;
            let mut want_connect = false;

            egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("PYIELINK EXPLORER").strong());
                    ui.separator();
                    if ui.button("⌂ Home (local)").clicked() {
                        ops_local.push(Op::Home(PaneKind::Local));
                    }
                    if ui.button("Refr").clicked() {
                        ops_remote.push(Op::Refresh(PaneKind::Remote));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Terminal").clicked() {
                            self.terminal.open = !self.terminal.open;
                        }
                        if ui.button("Disconnect").clicked() {
                            if let Some(s) = &self.session {
                                s.close();
                            }
                            self.session = None;
                            self.phase = Phase::Dialog;
                            self.connect.error = None;
                        }
                    });
                });
            });

            // Connect window
            egui::Window::new("Connect")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("user");
                        ui.text_edit_singleline(&mut self.connect.user);
                    });
                    ui.horizontal(|ui| {
                        ui.label("host");
                        ui.text_edit_singleline(&mut self.connect.ip);
                    });
                    ui.horizontal(|ui| {
                        ui.label("password");
                        ui.add(egui::TextEdit::singleline(&mut self.connect.password).password(true));
                    });
                    ui.checkbox(&mut self.connect.accept_license, "Accept license");
                    if let Some(err) = &self.connect.error {
                        ui.label(egui::RichText::new(err).color(Color32::RED));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let ok = ui.add_enabled(!self.connect.busy, egui::Button::new("Connect"));
                        if ok.clicked() {
                            want_connect = true;
                        }
                        if ui.button("Exit").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });

            // draw panes anyway so the layout doesn't jump
            egui::SidePanel::left("remote").resizable(true).default_width(620.0).show(ctx, |ui| {
                ui.label(egui::RichText::new("REMOTE").strong().color(Color32::from_rgb(120, 200, 255)));
                remote_rect = ui.available_rect_before_wrap();
                self.render_pane(ui, &mut ops_remote);
            });
            egui::SidePanel::right("local").resizable(true).default_width(540.0).show(ctx, |ui| {
                ui.label(egui::RichText::new("LOCAL").strong().color(Color32::from_rgb(255, 200, 120)));
                local_rect = ui.available_rect_before_wrap();
                self.render_pane(ui, &mut ops_local);
            });
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.label(egui::RichText::new("connect to a host to browse remote files").color(Color32::GRAY));
                ui.label(egui::RichText::new(&self.status).color(Color32::LIGHT_BLUE));
            });

            if want_connect {
                let target = format!("{}@{}", self.connect.user.trim(), self.connect.ip.trim());
                if self.connect.user.trim().is_empty() || self.connect.ip.trim().is_empty() {
                    self.connect.error = Some("user and host are required".to_string());
                } else {
                    self.connect.busy = true;
                    match GuiSession::connect(&target, &self.connect.password, self.connect.accept_license) {
                        Ok(session) => {
                            self.session = Some(session);
                            self.phase = Phase::Live;
                            self.connect.busy = false;
                            self.status("connecting...".to_string());
                        }
                        Err(e) => {
                            self.connect.error = Some(e);
                            self.connect.busy = false;
                        }
                    }
                }
            }

            for op in ops_local {
                if let Some(m) = self.handle_op(op) {
                    self.modal = Some(m);
                }
            }
            for op in ops_remote {
                if let Some(m) = self.handle_op(op) {
                    self.modal = Some(m);
                }
            }
            self.handle_keyboard(ctx);
            ctx.request_repaint();
            return;
        }

        // ------------------------------ LIVE phase ------------------------------
        let mut ops_local: Vec<Op> = Vec::new();
        let mut ops_remote: Vec<Op> = Vec::new();
        let mut remote_rect = egui::Rect::NOTHING;
        let mut local_rect = egui::Rect::NOTHING;
        let mut pull_sel = false;
        let mut push_sel = false;
        let mut paste_btn = false;

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("PYIELINK EXPLORER").strong());
                ui.separator();
                ui.label(egui::RichText::new(&self.status).color(Color32::LIGHT_BLUE));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Terminal").clicked() {
                        self.terminal.open = !self.terminal.open;
                    }
                    if ui.button("Paste").clicked() {
                        paste_btn = true;
                    }
                    if ui.button("Cut").clicked() {
                        ops_remote.push(Op::CutClip(self.focused));
                    }
                    if ui.button("Copy").clicked() {
                        ops_remote.push(Op::CopyClip(self.focused));
                    }
                    if ui.button("New").clicked() {
                        ops_remote.push(Op::NewFolder(self.focused));
                    }
                    if ui.button("Upload ↑").clicked() {
                        push_sel = true;
                    }
                    if ui.button("Download ↓").clicked() {
                        pull_sel = true;
                    }
                    if ui.button("Disconnect").clicked() {
                        if let Some(s) = &self.session {
                            s.close();
                        }
                        self.session = None;
                        self.phase = Phase::Dialog;
                        self.connect.error = None;
                    }
                });
            });
        });

        if self.terminal.open {
            egui::TopBottomPanel::bottom("terminal").resizable(true).default_height(180.0).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("TERMINAL").strong().color(Color32::GREEN));
                    ui.checkbox(&mut self.terminal.elevated, "sudo");
                    if ui.button("Clear").clicked() {
                        self.terminal.log.clear();
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                    for (line, color) in &self.terminal.log {
                        ui.label(egui::RichText::new(line).monospace().color(*color));
                    }
                });
                ui.horizontal(|ui| {
                    ui.add_enabled(!self.terminal.busy, egui::TextEdit::singleline(&mut self.terminal.input).hint_text("remote command..."));
                    if ui.button("Run").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let cmd = self.terminal.input.trim().to_string();
                        if !cmd.is_empty() && !self.terminal.busy {
                            if let Some(s) = &self.session {
                                let _ = s.send(GuiCommand::Exec {
                                    cmd,
                                    elevated: self.terminal.elevated,
                                });
                            }
                            self.terminal.busy = true;
                            self.terminal.input.clear();
                        }
                    }
                });
            });
        }

        egui::SidePanel::left("remote").resizable(true).default_width(620.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("REMOTE").strong().color(Color32::from_rgb(120, 200, 255)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Download ↓").clicked() { pull_sel = true; }
                });
            });
            remote_rect = ui.available_rect_before_wrap();
            self.render_pane(ui, &mut ops_remote);
        });

        egui::SidePanel::right("local").resizable(true).default_width(540.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("LOCAL").strong().color(Color32::from_rgb(255, 200, 120)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Upload ↑").clicked() { push_sel = true; }
                });
            });
            local_rect = ui.available_rect_before_wrap();
            self.render_pane(ui, &mut ops_local);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label(egui::RichText::new("drag files onto a pane to transfer · Ctrl+C/X/V to copy/cut/paste the focused pane · F-keys: none").color(Color32::GRAY));
        });

        // button-driven transfer of the FOCUSED pane selection
        if pull_sel {
            let item = self.pane(self.focused).selected_and_item();
            if let Some(it) = item {
                self.pull(it);
            }
        }
        if push_sel {
            let item = self.pane(self.focused).selected_and_item();
            if let Some(it) = item {
                self.push(it);
            }
        }
        if paste_btn {
            self.paste_to(self.focused);
        }

        for op in ops_local {
            if let Some(m) = self.handle_op(op) {
                self.modal = Some(m);
            }
        }
        for op in ops_remote {
            if let Some(m) = self.handle_op(op) {
                self.modal = Some(m);
            }
        }
        self.handle_dropped_files(ctx, remote_rect, local_rect);
        self.handle_dnd(ctx, remote_rect, local_rect);
        self.handle_keyboard(ctx);

        if let Some(m) = self.modal.take() {
            match m {
                Modal::Text { title, input, mode } => {
                    egui::Window::new(&title)
                        .collapsible(false)
                        .resizable(false)
                        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                        .show(ctx, |ui| {
                            let mut input = input.clone();
                            ui.text_edit_singleline(&mut input);
                            ui.horizontal(|ui| {
                                if ui.button("OK").clicked() {
                                    self.run_modal_text(Modal::Text { title, input: input.clone(), mode });
                                    *self.modal = None;
                                }
                                if ui.button("Cancel").clicked() {
                                    *self.modal = None;
                                }
                            });
                        });
                }
                Modal::Confirm { title, msg, action } => {
                    egui::Window::new(&title)
                        .collapsible(false)
                        .resizable(false)
                        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                        .show(ctx, |ui| {
                            ui.label(msg);
                            ui.horizontal(|ui| {
                                if ui.button("Delete").clicked() {
                                    self.run_confirm(action);
                                    *self.modal = None;
                                }
                                if ui.button("Cancel").clicked() {
                                    *self.modal = None;
                                }
                            });
                        });
                }
            }
        }

        ctx.request_repaint();
        let _ = (remote_rect, local_rect);
    }
}

impl Pane {
    fn selected_and_item(&self) -> Option<(String, bool)> {
        let idx = self.selected?;
        let e = self.entries.get(idx)?;
        Some((e.name.clone(), e.is_dir))
    }
}