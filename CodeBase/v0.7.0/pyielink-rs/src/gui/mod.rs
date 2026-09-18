//! Native file-explorer GUI for pyielink (egui/eframe).
//!
//! Launch via `pyielink user@ip explorer` (requires the `gui` feature).
//! The video layer is deliberately NOT part of this app — it is broken and
//! removed from the framework; this is a pure remote+local file explorer.

use std::path::{Path, PathBuf};

use eframe::egui::{self, Align2, Color32};

use crate::client::{normalize_remote, resolve_remote, stem_of, GuiCommand, GuiEvent, GuiSession};

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

#[derive(Clone)]
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

#[derive(Clone)]
enum TextMode {
    NewFolder(PaneKind),
    Rename(PaneKind, usize),
}

enum ConfirmAction {
    DeleteRemote { path: String, is_dir: bool },
    DeleteLocal { path: PathBuf },
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
        action: Option<ConfirmAction>,
    },
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

// --------------------------------- panes ---------------------------------

struct Pane {
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
    fn remote() -> Self {
        Pane {
            cwd_label: "~".to_string(),
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

    fn selected_item(&self) -> Option<(String, bool)> {
        let idx = self.selected?;
        let e = self.entries.get(idx)?;
        Some((e.name.clone(), e.is_dir))
    }
}

// --------------------------------- the app ---------------------------------

struct ExplorerApp {
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

#[derive(Clone, Copy, PartialEq, Eq)]
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
        let mut local = Pane::local(default_local_dir());
        load_local_listing(&mut local);
        ExplorerApp {
            phase: Phase::Dialog,
            connect: ConnectDialog {
                user,
                ip,
                accept_license: true,
                ..Default::default()
            },
            session: None,
            status: "not connected".to_string(),
            remote: Pane::remote(),
            local,
            focused: PaneKind::Remote,
            clipboard: None,
            sweep: None,
            terminal: TerminalPane::default(),
            modal: None,
            drag: None,
        }
    }

    fn pane(&self, kind: PaneKind) -> &Pane {
        match kind {
            PaneKind::Remote => &self.remote,
            PaneKind::Local => &self.local,
        }
    }

    fn pane_mut(&mut self, kind: PaneKind) -> &mut Pane {
        match kind {
            PaneKind::Remote => &mut self.remote,
            PaneKind::Local => &mut self.local,
        }
    }

    fn status(&mut self, text: String) {
        self.status = text;
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

    fn navigate(&mut self, kind: PaneKind, label: String) {
        let (remote_cwd, local_cwd) = match kind {
            PaneKind::Remote => {
                let cwd = if label == "~" { ".".to_string() } else { label.clone() };
                (cwd, PathBuf::new())
            }
            PaneKind::Local => {
                let cwd = PathBuf::from(&label);
                (String::new(), cwd)
            }
        };
        let pane = self.pane_mut(kind);
        pane.history.push(pane.cwd_label.clone());
        pane.cwd_label = label;
        pane.remote_cwd = remote_cwd;
        pane.local_cwd = local_cwd;
        pane.selected = None;
        let _ = pane;
        self.request_list(kind);
    }

    // ---------- transfers ----------

    fn pull(&mut self, name: String, is_dir: bool) {
        if is_dir {
            self.status(format!("'{}' is a directory — download is file-only for now", name));
            return;
        }
        let Some(s) = &self.session else {
            self.status("not connected".to_string());
            return;
        };
        let remote = self.remote.remote_path_for(&name);
        let local = self.local.local_cwd.join(&name);
        let _ = s.send(GuiCommand::Get { remote, local });
        self.status(format!("downloading {}...", name));
    }

    fn push(&mut self, name: String, is_dir: bool) {
        if is_dir {
            self.status(format!("'{}' is a directory — upload is file-only for now", name));
            return;
        }
        let Some(s) = &self.session else {
            self.status("not connected".to_string());
            return;
        };
        let local = self.local.local_cwd.join(&name);
        if !local.is_file() {
            self.status(format!("no such local file: {}", local.display()));
            return;
        }
        let remote = self.remote.remote_path_for(&name);
        let _ = s.send(GuiCommand::Put { local, remote });
        self.status(format!("uploading {}...", name));
    }

    // ---------- clipboard ----------

    fn copy_selection(&mut self, cut: bool) {
        let kind = self.focused;
        let item = match self.pane(kind).selected_item() {
            Some((name, is_dir)) => ClipItem { name, is_dir },
            None => {
                self.status("nothing selected to copy".to_string());
                return;
            }
        };
        let pane = self.pane(kind);
        self.clipboard = Some(Clipboard {
            kind,
            remote_cwd: pane.remote_cwd.clone(),
            local_cwd: pane.local_cwd.clone(),
            items: vec![item],
            cut,
        });
        self.status(format!(
            "{} '{}' to clipboard",
            if cut { "cut" } else { "copied" },
            self.clipboard.as_ref().unwrap().items[0].name
        ));
    }

    fn paste_to(&mut self, to: PaneKind) {
        let Some(clip) = self.clipboard.clone() else {
            self.status("clipboard is empty".to_string());
            return;
        };
        let (to_remote_cwd, to_local_cwd, dst_entries) = {
            let p = self.pane(to);
            (p.remote_cwd.clone(), p.local_cwd.clone(), p.entries.clone())
        };
        let mut sweep_targets: Vec<SweepTarget> = Vec::new();

        for item in &clip.items {
            match (clip.kind, to) {
                (PaneKind::Remote, PaneKind::Remote) => {
                    if let Some(s) = &self.session {
                        let from = resolve_remote(&clip.remote_cwd, &item.name);
                        let to_path = resolve_remote(
                            &to_remote_cwd,
                            &self.dedup_name(&dst_entries, &item.name, item.is_dir, !clip.cut),
                        );
                        let _ = s.send(GuiCommand::Copy { from: from.clone(), to: to_path });
                        if clip.cut {
                            sweep_targets.push(SweepTarget {
                                kind: PaneKind::Remote,
                                remote: from,
                                local: PathBuf::new(),
                            });
                        }
                    }
                }
                (PaneKind::Local, PaneKind::Local) => {
                    let src = clip.local_cwd.join(&item.name);
                    let dst = to_local_cwd.join(&self.dedup_name(&dst_entries, &item.name, item.is_dir, !clip.cut));
                    if clip.cut {
                        if std::fs::rename(&src, &dst).is_err() {
                            let _ = remove_local(&dst, true);
                            let _ = copy_local(&src, &dst, item.is_dir);
                            let _ = remove_local(&src, item.is_dir);
                        }
                    } else if let Err(e) = copy_local(&src, &dst, item.is_dir) {
                        self.status(format!("copy failed: {}", e));
                    }
                }
                (PaneKind::Remote, PaneKind::Local) => {
                    if let Some(s) = &self.session {
                        let remote = resolve_remote(&clip.remote_cwd, &item.name);
                        if item.is_dir {
                            self.status("can't download directories yet".to_string());
                        } else {
                            let local = to_local_cwd.join(&self.dedup_name(&dst_entries, &item.name, item.is_dir, !clip.cut));
let _ = s.send(GuiCommand::Get { remote: remote.clone(), local });
                            if clip.cut {
                                sweep_targets.push(SweepTarget {
                                    kind: PaneKind::Remote,
                                    remote,
                                    local: PathBuf::new(),
                                });
                            }
                        }
                    }
                }
                (PaneKind::Local, PaneKind::Remote) => {
                    if let Some(s) = &self.session {
                        let local = clip.local_cwd.join(&item.name);
                        if item.is_dir {
                            self.status("can't upload directories yet".to_string());
                        } else if local.is_file() {
                            let remote = resolve_remote(
                                &to_remote_cwd,
                                &self.dedup_name(&dst_entries, &item.name, false, !clip.cut),
                            );
                            let _ = s.send(GuiCommand::Put { local: local.clone(), remote });
                            if clip.cut {
                                sweep_targets.push(SweepTarget {
                                    kind: PaneKind::Local,
                                    remote: String::new(),
                                    local,
                                });
                            }
                        } else {
                            self.status(format!("no such local file: {}", local.display()));
                        }
                    }
                }
            }
        }

        if !sweep_targets.is_empty() {
            let remaining = sweep_targets.len();
            self.sweep = Some(PendingSweep { remaining, targets: sweep_targets });
        }
        self.request_list(to);
        if clip.kind != to {
            self.request_list(clip.kind);
        }
        self.status(format!(
            "{} {} -> {}",
            if clip.cut { "moved" } else { "copied" },
            if clip.kind == PaneKind::Remote { "remote" } else { "local" },
            if to == PaneKind::Remote { "remote" } else { "local" },
        ));
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
                    let _ = remove_local(&t.local, false);
                }
            }
        }
        self.status("cut sources removed".to_string());
    }

    // ---------- ops ----------

    fn handle_op(&mut self, op: Op) {
        match op {
            Op::Refresh(kind) => self.request_list(kind),
            Op::Up(kind) => match kind {
                PaneKind::Remote => {
                    let next = normalize_remote(&self.remote.remote_cwd, "..");
                    let label = if next == "." { "~".to_string() } else { next.clone() };
                    self.navigate(kind, label);
                }
                PaneKind::Local => {
                    if let Some(parent) = self.local.local_cwd.parent() {
                        self.navigate(kind, parent.display().to_string());
                    }
                }
            },
            Op::Back(kind) => {
                if let Some(label) = self.pane_mut(kind).history.pop() {
                    self.navigate(kind, label);
                }
            }
            Op::Home(kind) => match kind {
                PaneKind::Remote => self.navigate(kind, "~".to_string()),
                PaneKind::Local => self.navigate(kind, default_local_dir().display().to_string()),
            },
            Op::Enter(kind, idx) => {
                self.focused = kind;
                let entry = self.pane(kind).entries.get(idx).cloned();
                if let Some(e) = entry {
                    if e.is_dir {
                        match kind {
                            PaneKind::Remote => {
                                let next = self.remote.remote_path_for(&e.name);
                                self.navigate(kind, next);
                            }
                            PaneKind::Local => {
                                let p = self.local.local_cwd.join(&e.name);
                                self.navigate(kind, p.display().to_string());
                            }
                        }
                    }
                }
            }
            Op::NewFolder(kind) => {
                self.focused = kind;
                self.modal = Some(Modal::Text {
                    title: "New folder".to_string(),
                    input: String::new(),
                    mode: TextMode::NewFolder(kind),
                });
            }
            Op::Rename(kind, idx) => {
                self.focused = kind;
                let name = self.pane(kind).entries.get(idx).map(|e| e.name.clone()).unwrap_or_default();
                self.modal = Some(Modal::Text {
                    title: "Rename".to_string(),
                    input: name,
                    mode: TextMode::Rename(kind, idx),
                });
            }
            Op::Delete(kind, idx) => {
                self.focused = kind;
                let e = self.pane(kind).entries.get(idx).cloned();
                if let Some(e) = e {
                    let action = match kind {
                        PaneKind::Remote => ConfirmAction::DeleteRemote {
                            path: self.remote.remote_path_for(&e.name),
                            is_dir: e.is_dir,
                        },
                        PaneKind::Local => ConfirmAction::DeleteLocal {
                            path: self.local.local_cwd.join(&e.name),
                        },
                    };
                    self.modal = Some(Modal::Confirm {
                        title: "Delete".to_string(),
                        msg: format!("Delete '{}'?", e.name),
                        action: Some(action),
                    });
                }
            }
            Op::CopyClip(kind) | Op::CutClip(kind) => {
                self.focused = kind;
                let cut = matches!(op, Op::CutClip(_));
                self.copy_selection(cut);
            }
        }
    }

    fn run_modal_text(&mut self, input: String, mode: TextMode) {
        let input = input.trim().to_string();
        if input.is_empty() {
            self.status("name must not be empty".to_string());
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
                        Err(e) => self.status(format!("mkdir failed: {}", e)),
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
                            Err(e) => self.status(format!("rename failed: {}", e)),
                        }
                    }
                }
            }
        }
    }

    fn run_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::DeleteRemote { path, is_dir } => {
                if let Some(s) = &self.session {
                    let _ = s.send(GuiCommand::Delete { path, recursive: is_dir });
                }
                self.request_list(PaneKind::Remote);
            }
            ConfirmAction::DeleteLocal { path } => {
                let is_dir = path.is_dir();
                if remove_local(&path, is_dir).is_err() {
                    self.status(format!("delete failed: {}", path.display()));
                }
                self.request_list(PaneKind::Local);
            }
        }
    }

    // ---------- events ----------

    fn poll_events(&mut self) {
        let events: Vec<GuiEvent> = {
            let Some(session) = &self.session else { return };
            std::iter::from_fn(|| session.try_event()).collect()
        };
        for ev in events {
            match ev {
                GuiEvent::Ready => {
                    self.status("connected — loading remote...".to_string());
                    self.request_list(PaneKind::Remote);
                }
                GuiEvent::DirListing(listing) => {
                    if listing.path == self.remote.remote_cwd {
                        let mut entries = listing
                            .entries
                            .iter()
                            .map(|e| Entry {
                                name: e.name.clone(),
                                is_dir: e.is_dir,
                                size: e.size,
                            })
                            .collect::<Vec<Entry>>();
                        sort_entries(&mut entries);
                        self.remote.entries = entries;
                        self.remote.loading = false;
                        self.remote.error = None;
                    }
                }
                GuiEvent::OpResult { op, ok, msg } => {
                    if ok {
                        self.status(format!("remote {} ok", op));
                        self.request_list(PaneKind::Remote);
                        if op == "copy" {
                            let reached_zero = self.sweep.as_mut().is_some_and(|s| {
                                if s.remaining > 0 {
                                    s.remaining -= 1;
                                }
                                s.remaining == 0
                            });
                            if reached_zero {
                                self.apply_sweep();
                            }
                        }
                    } else {
                        self.status(format!("remote {} failed: {}", op, msg));
                    }
                }
                GuiEvent::TransferDone { label, ok } => {
                    if ok {
                        let reached_zero = self.sweep.as_mut().is_some_and(|s| {
                            if s.remaining > 0 {
                                s.remaining -= 1;
                            }
                            s.remaining == 0
                        });
                        if reached_zero {
                            self.apply_sweep();
                        }
                    }
                    self.status(format!("transfer '{}' {}", label, if ok { "OK" } else { "FAILED" }));
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
                    self.status(if r.is_empty() { "disconnected".to_string() } else { format!("disconnected: {}", r) });
                }
            }
        }
    }

    // ---------- drops / keyboard ----------

    fn handle_dropped_files(&mut self, ctx: &egui::Context, local_rect: egui::Rect) {
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
        let over_local = ctx
            .input(|i| i.pointer.hover_pos())
            .map(|p| local_rect.contains(p))
            .unwrap_or(false);
        if over_local {
            for p in &dropped {
                let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let dst = self.local.local_cwd.join(&name);
                if copy_local(p, &dst, false).is_err() {
                    self.status(format!("could not copy {}", p.display()));
                }
            }
            self.request_list(PaneKind::Local);
            self.status(format!("copied {} file(s) into local", dropped.len()));
        } else if self.session.is_some() {
            for p in &dropped {
                let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let remote = self.remote.remote_path_for(&name);
                let _ = self.session.as_ref().unwrap().send(GuiCommand::Put {
                    local: p.clone(),
                    remote,
                });
            }
            self.status(format!("uploading {} file(s)...", dropped.len()));
        }
    }

    fn handle_dnd(&mut self, ctx: &egui::Context, remote_rect: egui::Rect, local_rect: egui::Rect) {
        let Some(drag) = self.drag.clone() else { return };
        let released = ctx.input(|i| i.pointer.any_released());
        if !released {
            return;
        }
        self.drag = None;
        let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else { return };
        if drag.kind == PaneKind::Remote && local_rect.contains(pos) {
            let name = drag.name.clone();
            let is_dir = drag.is_dir;
            if let Some(s) = &self.session {
                if !is_dir {
                    let remote = self.remote.remote_path_for(&name);
                    let local = self.local.local_cwd.join(&self.dedup_name(&self.local.entries, &name, false, true));
                    let _ = s.send(GuiCommand::Get { remote, local });
                    self.status(format!("downloading {}...", name));
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
                    self.status(format!("uploading {}...", name));
                }
            }
        }
    }

    fn handle_keyboard(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let (copy, cut, paste) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::C) && i.modifiers.command,
                i.key_pressed(egui::Key::X) && i.modifiers.command,
                i.key_pressed(egui::Key::V) && i.modifiers.command,
            )
        });
        let kind = self.focused;
        if copy {
            self.copy_selection(false);
        } else if cut {
            self.copy_selection(true);
        } else if paste {
            self.paste_to(kind);
        }
    }

    // ---------- pane widget ----------

    fn render_pane(&mut self, kind: PaneKind, ui: &mut egui::Ui, ops: &mut Vec<Op>) {
        let (label, loading, error, entries, selected) = {
            let p = self.pane(kind);
            (
                p.cwd_label.clone(),
                p.loading,
                p.error.clone(),
                p.entries.clone(),
                p.selected,
            )
        };
        ui.horizontal(|ui| {
            if ui.button("←").on_hover_text("back").clicked() {
                ops.push(Op::Back(kind));
            }
            if ui.button("↑").on_hover_text("up").clicked() {
                ops.push(Op::Up(kind));
            }
            if ui.button("⌂").on_hover_text("home/root").clicked() {
                ops.push(Op::Home(kind));
            }
            if ui.button("⟳").on_hover_text("refresh").clicked() {
                ops.push(Op::Refresh(kind));
            }
            ui.separator();
            ui.label(egui::RichText::new(&label).strong().size(14.0));
        });
        ui.separator();
        if loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("loading...");
            });
        }
        if let Some(err) = &error {
            ui.label(egui::RichText::new(err).color(Color32::RED));
        }
        let mut new_sel = selected;
        let mut drag: Option<DragState> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, e) in entries.iter().enumerate() {
                let is_sel = new_sel == Some(i);
                let icon = if e.is_dir { "·[DIR]·" } else { "·" };
                let size_txt = if e.is_dir { "<dir>".to_string() } else { format!("{} B", e.size) };
                let text = format!("{} {}  {:>12}", icon, e.name, size_txt);
                let resp = ui.selectable_label(is_sel, egui::RichText::new(text).monospace());
                if resp.clicked() {
                    new_sel = Some(i);
                }
                if resp.double_clicked() {
                    ops.push(Op::Enter(kind, i));
                }
                if resp.drag_started() {
                    drag = Some(DragState {
                        kind,
                        name: e.name.clone(),
                        is_dir: e.is_dir,
                    });
                }
                resp.context_menu(|ui| {
                    if e.is_dir && ui.button("Open").clicked() {
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
                        new_sel = Some(i);
                        ops.push(Op::CopyClip(kind));
                        ui.close_menu();
                    }
                    if ui.button("Cut").clicked() {
                        new_sel = Some(i);
                        ops.push(Op::CutClip(kind));
                        ui.close_menu();
                    }
                });
            }
        });
        self.pane_mut(kind).selected = new_sel;
        if let Some(d) = drag {
            self.drag = Some(d);
        }
    }
}

// --------------------------------- helpers ---------------------------------

fn default_local_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn sort_entries(entries: &mut Vec<Entry>) {
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a
            .name
            .to_lowercase()
            .cmp(&b.name.to_lowercase()),
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
        pane.entries.push(Entry { name, is_dir, size });
    }
    sort_entries(&mut pane.entries);
}

fn copy_local(src: &Path, dst: &Path, is_dir: bool) -> Result<(), String> {
    if is_dir {
        std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
        for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            copy_local(&entry.path(), &dst.join(entry.file_name()), is_dir)?;
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

        // ---- shared toolbar ----
        let mut toolbar_ops: Vec<Op> = Vec::new();
        let mut pull_sel = false;
        let mut push_sel = false;
        let mut paste_btn = false;
        let mut disconnect = false;
        let mut terminal_toggle = false;

        if !matches!(self.phase, Phase::Dialog) {
            egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("PYIELINK EXPLORER").strong());
                    ui.separator();
                    ui.label(egui::RichText::new(&self.status).color(Color32::LIGHT_BLUE));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Terminal").clicked() {
                            terminal_toggle = true;
                        }
                        if ui.button("Paste").clicked() {
                            paste_btn = true;
                        }
                        if ui.button("Cut").clicked() {
                            toolbar_ops.push(Op::CutClip(self.focused));
                        }
                        if ui.button("Copy").clicked() {
                            toolbar_ops.push(Op::CopyClip(self.focused));
                        }
                        if ui.button("New").clicked() {
                            toolbar_ops.push(Op::NewFolder(self.focused));
                        }
                        if ui.button("Upload ↑").clicked() {
                            push_sel = true;
                        }
                        if ui.button("Download ↓").clicked() {
                            pull_sel = true;
                        }
                        if ui.button("Disconnect").clicked() {
                            disconnect = true;
                        }
                    });
                });
            });
        }

        if disconnect {
            if let Some(s) = &self.session {
                s.close();
            }
            self.session = None;
            self.phase = Phase::Dialog;
            self.connect.busy = false;
            self.connect.error = None;
        }
        if terminal_toggle {
            self.terminal.open = !self.terminal.open;
        }

        // ---- terminal ----
        if self.terminal.open {
            egui::TopBottomPanel::bottom("terminal")
                .resizable(true)
                .default_height(180.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("TERMINAL").strong().color(Color32::GREEN));
                        ui.checkbox(&mut self.terminal.elevated, "sudo");
                        if ui.button("Clear").clicked() {
                            self.terminal.log.clear();
                        }
                    });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for (line, color) in &self.terminal.log {
                                ui.label(egui::RichText::new(line).monospace().color(*color));
                            }
                        });
                    ui.horizontal(|ui| {
                        let run = ui.add_enabled(
                            !self.terminal.busy,
                            egui::TextEdit::singleline(&mut self.terminal.input).hint_text("remote command..."),
                        );
                        let enter = run.lost_focus()
                            && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter));
                        let clicked = ui.button("Run").clicked();
                        if (clicked || enter) && !self.terminal.input.trim().is_empty() && !self.terminal.busy {
                            let cmd = self.terminal.input.trim().to_string();
                            if let Some(s) = &self.session {
                                let _ = s.send(GuiCommand::Exec {
                                    cmd,
                                    elevated: self.terminal.elevated,
                                });
                            }
                            self.terminal.busy = true;
                            self.terminal.input.clear();
                        }
                    });
                });
        }

        // ---- panes ----
        let mut ops_remote: Vec<Op> = Vec::new();
        let mut ops_local: Vec<Op> = Vec::new();
        let remote_rect = {
            let resp = egui::SidePanel::left("remote")
                .resizable(true)
                .default_width(620.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("REMOTE").strong().color(Color32::from_rgb(120, 200, 255)));
                        ui.separator();
                    });
                    if self.phase == Phase::Dialog {
                        ui.label(egui::RichText::new("— not connected —").color(Color32::GRAY));
                    }
                    self.render_pane(PaneKind::Remote, ui, &mut ops_remote);
                });
            resp.response.rect
        };

        let local_rect = {
            let resp = egui::SidePanel::right("local")
                .resizable(true)
                .default_width(540.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("LOCAL").strong().color(Color32::from_rgb(255, 200, 120)));
                        ui.separator();
                    });
                    self.render_pane(PaneKind::Local, ui, &mut ops_local);
                });
            resp.response.rect
        };

        // ---- central status ----
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label(egui::RichText::new("drag files onto a pane to transfer · Ctrl+C/X/V copy/cut/paste the focused pane · right-click entries for more").color(Color32::GRAY));
        });

        // ---- connect dialog ----
        if let Phase::Dialog = self.phase {
            let mut want_connect = false;
            egui::Window::new("Connect")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("user@");
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
                    ui.checkbox(&mut self.connect.accept_license, "Accept the license agreement");
                    if let Some(err) = &self.connect.error {
                        ui.label(egui::RichText::new(err).color(Color32::RED));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!self.connect.busy, egui::Button::new("Connect")).clicked() {
                            want_connect = true;
                        }
                        if ui.button("Exit").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });
            if want_connect {
                let user = self.connect.user.trim();
                let ip = self.connect.ip.trim();
                if user.is_empty() || ip.is_empty() {
                    self.connect.error = Some("user and host are required".to_string());
                } else {
                    self.connect.busy = true;
                    let target = format!("{}@{}", user, ip);
                    match GuiSession::connect(&target, &self.connect.password, self.connect.accept_license) {
                        Ok(session) => {
                            self.session = Some(session);
                            self.phase = Phase::Live;
                            self.connect.busy = false;
                            self.connect.error = None;
                            self.status("connecting...".to_string());
                        }
                        Err(e) => {
                            self.connect.error = Some(e);
                            self.connect.busy = false;
                        }
                    }
                }
            }
        }

        // ---- transfer buttons (focused pane) ----
        if pull_sel {
            if let Some((name, is_dir)) = self.pane(self.focused).selected_item() {
                self.pull(name, is_dir);
            }
        }
        if push_sel {
            if let Some((name, is_dir)) = self.pane(self.focused).selected_item() {
                self.push(name, is_dir);
            }
        }
        if paste_btn {
            self.paste_to(self.focused);
        }

        // ---- process collected ops ----
        for op in ops_local {
            self.handle_op(op);
        }
        for op in ops_remote {
            self.handle_op(op);
        }
        for op in toolbar_ops {
            self.handle_op(op);
        }

        // ---- drops ----
        self.handle_dropped_files(ctx, local_rect);
        self.handle_dnd(ctx, remote_rect, local_rect);
        self.handle_keyboard(ctx);

        // ---- modals (persistent) ----
        let mut ok_pressed = false;
        let mut cancel_pressed = false;
        match &self.modal {
            Some(Modal::Text { title, input: _input, .. }) => {
                let title = title.clone();
                egui::Window::new(&title)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        if let Some(Modal::Text { input, .. }) = &mut self.modal {
                            ui.text_edit_singleline(input);
                        }
                        ui.horizontal(|ui| {
                            if ui.button("OK").clicked() {
                                ok_pressed = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancel_pressed = true;
                            }
                        });
                    });
            }
            Some(Modal::Confirm { title, msg, .. }) => {
                let title = title.clone();
                let msg = msg.clone();
                egui::Window::new(&title)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.label(msg);
                        ui.horizontal(|ui| {
                            if ui.button("Delete").clicked() {
                                ok_pressed = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancel_pressed = true;
                            }
                        });
                    });
            }
            None => {}
        }
        if ok_pressed {
            match self.modal.take() {
                Some(Modal::Text { input, mode, .. }) => {
                    self.run_modal_text(input, mode);
                }
                Some(Modal::Confirm { action, .. }) => {
                    if let Some(a) = action {
                        self.run_confirm(a);
                    }
                }
                None => {}
            }
        } else if cancel_pressed {
            self.modal = None;
        }

        ctx.request_repaint();
    }
}