//! The Tern screen surface: source variables on the far left, the source
//! field and the pipeline result side by side, result variables on the far
//! right, and the operation tree over the script pipeline underneath.
//! This surface is NetCDF only.
//!
//! All data work happens on the worker thread. This file owns input, project
//! state and views; it never opens a dataset or evaluates a graph.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde_json::Value;
use tern_sdk::{
    keys::Key,
    ui::{self, html, Actions, Bound, Extent, Node, View},
    wire::{feature, Event},
    Error, Input, Options, Session, Surface, SurfaceOptions,
};

use crate::{
    graph,
    model::{ColorMap, DatasetInfo, Project, Rendered, ResultVariable, Series, Slice, NodeGraph, GraphNode, GraphLayer, Operation},
    runtime::{is_field, Completion, Output, Worker},
};

const CSS: &str = include_str!("../ui.css");
const DEBOUNCE: Duration = Duration::from_millis(250);
const PLAY_INTERVAL: Duration = Duration::from_millis(220);
const POLL: Duration = Duration::from_millis(120);

/// Run the interactive surface. Outside Tern it prints the usage and keys.
pub fn run(path: Option<String>, project_path: Option<String>) -> Result<()> {
    let options = Options::new()
        .app("tern-netcdf")
        .version(env!("CARGO_PKG_VERSION"))
        .feature(feature::EDIT)
        .feature(feature::UNDO)
        .feature(feature::SEND);
    let Some(mut session) = Session::<Msg>::connect(options)? else {
        usage();
        return Ok(());
    };
    let sf = session.open(
        SurfaceOptions::screen()
            .title("NetCDF")
            .role("netcdf"),
    )?;
    session.stylesheet(sf, "main", Some(CSS))?;

    let mut app = App::new(path, project_path);
    app.needs_render = true;
    let outcome = app.run(&mut session, sf);
    let _ = session.close_surface(sf, false);
    session.close()?;
    outcome
}

fn usage() {
    println!("NetCDF: a Tern screen surface for NetCDF fields.");
    println!("usage: tern-netcdf view [FILE.nc] [--project PATH]");
    println!();
    println!("keys");
    println!("  Ctrl+Enter  run the pipeline from any focus");
    println!("  Tab         next field; Shift+Tab previous");
    println!("  Esc         clear field focus; again quits");
    println!("  Ctrl+Q      quit");
    println!("  arrows      pan; + / - zoom; [ / ] step the result frame; space play");
}

#[derive(Clone, Debug)]
enum Msg {
    Act(String, Option<String>),
    Edit {
        key: String,
        from: usize,
        to: usize,
        text: String,
        cursor: usize,
        len: usize,
    },
    Undo {
        key: String,
    },
    Send {
        key: String,
        text: String,
    },
    Focus {
        key: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Unknown,
    Field,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Unknown => "no source",
            Self::Field => "NetCDF field",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Source,
    Result,
}

impl Side {
    fn key(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Result => "result",
        }
    }
}

struct Notice {
    text: String,
    ok: bool,
    busy: bool,
}

impl Notice {
    fn ok(text: impl Into<String>) -> Self {
        Self { text: text.into(), ok: true, busy: false }
    }
    fn busy(text: impl Into<String>) -> Self {
        Self { text: text.into(), ok: true, busy: true }
    }
    fn err(text: impl Into<String>) -> Self {
        Self { text: text.into(), ok: false, busy: false }
    }
}

/// A text field's model. Offsets are UTF-16 units, as Tern reports them.
struct Field {
    text: String,
    cursor: usize,
    anchor: usize,
    undo: Vec<(String, usize)>,
}

impl Field {
    fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let end = utf16_len(&text);
        Self { text, cursor: end, anchor: end, undo: Vec::new() }
    }

    fn set_text(&mut self, text: String) {
        self.cursor = utf16_len(&text);
        self.anchor = self.cursor;
        self.text = text;
    }

    fn push_undo(&mut self) {
        self.undo.push((self.text.clone(), self.cursor));
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
    }

    fn undo(&mut self) -> bool {
        let Some((text, cursor)) = self.undo.pop() else {
            return false;
        };
        self.text = text;
        self.cursor = cursor.min(utf16_len(&self.text));
        self.anchor = self.cursor;
        true
    }

    fn apply_edit(&mut self, from: usize, to: usize, text: &str, cursor: usize, len: usize) -> bool {
        if len != utf16_len(&self.text) || from > to {
            self.cursor = cursor.min(utf16_len(&self.text));
            self.anchor = self.cursor;
            return false;
        }
        let start = byte_at_utf16(&self.text, from);
        let end = byte_at_utf16(&self.text, to);
        if start > end || end > self.text.len() {
            return false;
        }
        if from == to && text.is_empty() {
            self.cursor = cursor.min(utf16_len(&self.text));
            self.anchor = self.cursor;
            return false;
        }
        self.push_undo();
        let mut next = String::with_capacity(self.text.len() + text.len());
        next.push_str(&self.text[..start]);
        next.push_str(text);
        next.push_str(&self.text[end..]);
        self.text = next;
        self.cursor = cursor.min(utf16_len(&self.text));
        self.anchor = self.cursor;
        true
    }

    fn insert(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        self.replace_selection(text)
    }

    fn replace_selection(&mut self, text: &str) -> bool {
        let len = utf16_len(&self.text);
        let a = self.cursor.min(len);
        let b = self.anchor.min(len);
        let (from, to) = if a <= b { (a, b) } else { (b, a) };
        let start = byte_at_utf16(&self.text, from);
        let end = byte_at_utf16(&self.text, to);
        self.push_undo();
        let mut next = String::with_capacity(self.text.len() + text.len());
        next.push_str(&self.text[..start]);
        next.push_str(text);
        next.push_str(&self.text[end..]);
        self.text = next;
        self.cursor = from + utf16_len(text);
        self.anchor = self.cursor;
        true
    }

    fn backspace(&mut self, word: bool) -> bool {
        if self.cursor != self.anchor {
            return self.replace_selection("");
        }
        if self.cursor == 0 {
            return false;
        }
        let at = byte_at_utf16(&self.text, self.cursor);
        let start = if word { word_start(&self.text, at) } else { prev_char_byte(&self.text, at) };
        let from = utf16_len(&self.text[..start]);
        let end = byte_at_utf16(&self.text, self.cursor);
        self.push_undo();
        let mut next = String::with_capacity(self.text.len());
        next.push_str(&self.text[..start]);
        next.push_str(&self.text[end..]);
        self.text = next;
        self.cursor = from;
        self.anchor = from;
        true
    }

    fn delete_forward(&mut self, word: bool) -> bool {
        if self.cursor != self.anchor {
            return self.replace_selection("");
        }
        if self.cursor >= utf16_len(&self.text) {
            return false;
        }
        let at = byte_at_utf16(&self.text, self.cursor);
        let end = if word { word_end(&self.text, at) } else { next_char_byte(&self.text, at) };
        let to = utf16_len(&self.text[..end]);
        let from = self.cursor;
        let start = byte_at_utf16(&self.text, from);
        let end = byte_at_utf16(&self.text, to);
        self.push_undo();
        let mut next = String::with_capacity(self.text.len());
        next.push_str(&self.text[..start]);
        next.push_str(&self.text[end..]);
        self.text = next;
        self.cursor = from;
        self.anchor = from;
        true
    }

    fn left(&mut self, word: bool) {
        if self.cursor != self.anchor {
            self.cursor = self.cursor.min(self.anchor);
            self.anchor = self.cursor;
            return;
        }
        let at = byte_at_utf16(&self.text, self.cursor);
        let start = if word { word_start(&self.text, at) } else { prev_char_byte(&self.text, at) };
        self.cursor = utf16_len(&self.text[..start]);
        self.anchor = self.cursor;
    }

    fn right(&mut self, word: bool) {
        if self.cursor != self.anchor {
            self.cursor = self.cursor.max(self.anchor);
            self.anchor = self.cursor;
            return;
        }
        let at = byte_at_utf16(&self.text, self.cursor);
        let end = if word { word_end(&self.text, at) } else { next_char_byte(&self.text, at) };
        self.cursor = utf16_len(&self.text[..end]);
        self.anchor = self.cursor;
    }

    fn home(&mut self) {
        let at = byte_at_utf16(&self.text, self.cursor);
        let (start, _) = line_bounds(&self.text, at);
        self.cursor = utf16_len(&self.text[..start]);
        self.anchor = self.cursor;
    }

    fn end(&mut self) {
        let at = byte_at_utf16(&self.text, self.cursor);
        let (_, end) = line_bounds(&self.text, at);
        self.cursor = utf16_len(&self.text[..end]);
        self.anchor = self.cursor;
    }
}

struct PaneView {
    rendered: Arc<Rendered>,
    blob: Option<String>,
}

struct App {
    worker: Worker,
    project: Project,
    project_path: Option<PathBuf>,
    open_field: Field,
    project_field: Field,
    dest_field: Field,
    src_ix_field: Field,
    src_iy_field: Field,
    src_frame_field: Field,
    src_min_field: Field,
    src_max_field: Field,
    dst_ix_field: Field,
    dst_iy_field: Field,
    dst_frame_field: Field,
    dst_min_field: Field,
    dst_max_field: Field,
    script: Field,
    kind: Kind,
    info: Option<DatasetInfo>,
    slice: Slice,
    source_slice: Slice,
    linked: bool,
    source_view: Option<PaneView>,
    result_view: Option<PaneView>,
    results: Vec<ResultVariable>,
    series: Option<Series>,
    slot_a: Option<String>,
    slot_b: Option<String>,
    selected_node: Option<u64>,
    connect_from: Option<u64>,
    colormap: ColorMap,
    opacity: f32,
    stride: usize,
    arrow_scale: f32,
    zoom: i32,
    pan_x: i32,
    pan_y: i32,
    playing: Option<Side>,
    play_due: Instant,
    dirty: bool,
    dirty_at: Option<Instant>,
    newest_data_gen: u64,
    export_pending: Option<u64>,
    last_ms: f64,
    status: Notice,
    focused: Option<String>,
    focused_id: Option<String>,
    needs_render: bool,
    quit: bool,
}

impl App {
    fn new(path: Option<String>, project_path: Option<String>) -> Self {
        let mut app = Self::empty();
        if let Some(pp) = project_path {
            app.project_field = Field::new(pp.clone());
            app.project_path = Some(PathBuf::from(&pp));
            if Path::new(&pp).is_file() {
                match app.load_from(&pp) {
                    Ok(()) => return app,
                    Err(err) => app.status = Notice::err(format!("Load project: {err:#}")),
                }
            }
        }
        if let Some(path) = path {
            app.set_source(path);
        }
        app
    }

    fn empty() -> Self {
        Self {
            worker: Worker::new(),
            project: Project::default(),
            project_path: None,
            open_field: Field::new(""),
            project_field: Field::new(""),
            dest_field: Field::new(""),
            src_ix_field: Field::new("0"),
            src_iy_field: Field::new("0"),
            src_frame_field: Field::new("1"),
            src_min_field: Field::new(""),
            src_max_field: Field::new(""),
            dst_ix_field: Field::new("0"),
            dst_iy_field: Field::new("0"),
            dst_frame_field: Field::new("1"),
            dst_min_field: Field::new(""),
            dst_max_field: Field::new(""),
            script: Field::new(""),
            kind: Kind::Unknown,
            info: None,
            slice: Slice::default(),
            source_slice: Slice::default(),
            linked: true,
            source_view: None,
            result_view: None,
            results: Vec::new(),
            series: None,
            slot_a: None,
            slot_b: None,
            selected_node: None,
            connect_from: None,
            colormap: ColorMap::Viridis,
            opacity: 1.0,
            stride: 8,
            arrow_scale: 1.0,
            zoom: 3,
            pan_x: 0,
            pan_y: 0,
            playing: None,
            play_due: Instant::now(),
            dirty: false,
            dirty_at: None,
            newest_data_gen: 0,
            export_pending: None,
            last_ms: 0.0,
            status: Notice::ok("Open a NetCDF file or type its path."),
            focused: None,
            focused_id: None,
            needs_render: true,
            quit: false,
        }
    }

    fn run(&mut self, session: &mut Session<Msg>, sf: Surface) -> Result<()> {
        loop {
            while let Some(completion) = self.worker.try_recv() {
                self.apply_completion(completion, session)?;
            }
            self.tick();
            if self.quit {
                break;
            }
            if self.needs_render {
                match session.render(sf, self.view()) {
                    Ok(()) => self.needs_render = false,
                    Err(Error::SurfaceClosed(_)) => break,
                    Err(err) => return Err(err.into()),
                }
            }
            match session.next(Some(self.wait())) {
                Ok(Some(Input::Msg(msg, _))) => {
                    self.handle_msg(msg, session, sf)?;
                }
                Ok(Some(Input::Key(key))) => {
                    if self.handle_key(&key, session, sf)? {
                        break;
                    }
                }
                Ok(Some(Input::Event(event))) => {
                    self.handle_event(event, session, sf);
                }
                Ok(None) => {}
                Err(Error::InputClosed) => break,
                Err(Error::SurfaceClosed(_)) => break,
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }

    fn wait(&self) -> Duration {
        let now = Instant::now();
        let mut wait = POLL;
        if self.dirty {
            if let Some(at) = self.dirty_at {
                wait = wait.min(at.saturating_duration_since(now));
            }
        }
        if self.playing.is_some() {
            wait = wait.min(self.play_due.saturating_duration_since(now));
        }
        wait.max(Duration::from_millis(10))
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if let Some(side) = self.playing {
            if self.kind == Kind::Field && now >= self.play_due {
                self.frame_step(side, 1);
                self.play_due = now + PLAY_INTERVAL;
            }
        }
        if self.dirty && self.dirty_at.is_some_and(|at| now >= at) {
            self.submit_now();
        }
    }

    fn handle_event(&mut self, event: Event, session: &Session<Msg>, sf: Surface) {
        match event {
            Event::Error(err) => {
                self.status = Notice::err(format!("Tern: {}", err.msg));
                self.needs_render = true;
            }
            Event::Gone(gone) => {
                let id = session.surface_id(sf);
                if gone.sf.as_deref() == Some(id) || gone.ids.iter().any(|dropped| dropped == id) {
                    self.quit = true;
                }
            }
            _ => {}
        }
    }

    fn handle_msg(&mut self, msg: Msg, session: &mut Session<Msg>, sf: Surface) -> Result<()> {
        match msg {
            Msg::Act(act, value) => self.act(&act, value.as_deref(), session, sf)?,
            Msg::Edit { key, from, to, text, cursor, len } => {
                let changed = match self.field_mut(&key) {
                    Some(field) => field.apply_edit(from, to, &text, cursor, len),
                    None => false,
                };
                if changed && key == "script" {
                    self.script_edited(true);
                } else {
                    self.needs_render = true;
                }
            }
            Msg::Undo { key } => {
                let changed = self.field_mut(&key).is_some_and(Field::undo);
                if changed && key == "script" {
                    self.script_edited(true);
                } else if changed {
                    self.needs_render = true;
                }
            }
            Msg::Send { key, text } => {
                if let Some(field) = self.field_mut(&key) {
                    field.set_text(text);
                }
                self.submit_field(&key);
            }
            Msg::Focus { key } => {
                self.focus_key(&key, session, sf)?;
            }
        }
        Ok(())
    }

    fn act(
        &mut self,
        act: &str,
        value: Option<&str>,
        session: &mut Session<Msg>,
        sf: Surface,
    ) -> Result<()> {
        self.clear_focus(session, sf)?;
        match act {
            "graph-add" | "graph-select" | "graph-output" | "graph-input" | "graph-remove" | "graph-param" | "graph-move" | "layer-map" | "layer-opacity" | "layer-toggle" | "layer-order" | "layer-add" | "layer-remove" => {
                if let Err(error) = self.graph_action(act, value.unwrap_or("")) {
                    self.status = Notice::err(format!("{error:#}"));
                }
                self.needs_render = true;
            }
            "open" => {
                let path = self.open_field.text.trim().to_owned();
                if path.is_empty() {
                    self.status = Notice::err("Type a data file path first.");
                } else {
                    self.set_source(path);
                }
            }
            "load" => {
                let path = self.project_field.text.trim().to_owned();
                if path.is_empty() {
                    self.status = Notice::err("Type a project path first.");
                } else {
                    match self.load_from(&path) {
                        Ok(()) => self.status = Notice::ok(format!("Loaded {path}")),
                        Err(err) => self.status = Notice::err(format!("Load project: {err:#}")),
                    }
                }
            }
            "save" => {
                let path = self.project_field.text.trim().to_owned();
                if path.is_empty() {
                    self.status = Notice::err("Type a project path first.");
                } else {
                    match self.save_to(&path) {
                        Ok(()) => self.status = Notice::ok(format!("Saved {path}")),
                        Err(err) => self.status = Notice::err(format!("Save project: {err:#}")),
                    }
                }
            }
            "run" => {
                self.project.script = self.script.text.clone();
                self.submit_now();
            }
            "source" => {
                if let Some(name) = value {
                    self.choose_source(name);
                }
            }
            "result" => self.choose_result(value.unwrap_or("")),
            "varb" => {
                if let Some(name) = value {
                    self.choose_second(name);
                }
            }
            "srccmap" => {
                if let Some(name) = value {
                    self.project.source_colormap = color_map(name);
                    self.mark_dirty(false);
                }
            }
            "range" => {
                if let Some(side) = value.and_then(side_of) {
                    self.apply_range(side);
                }
            }
            "rangeauto" => {
                if let Some(side) = value.and_then(side_of) {
                    match side {
                        Side::Source => self.project.source_range = None,
                        Side::Result => self.project.result_range = None,
                    }
                    self.sync_range_fields();
                    self.mark_dirty(false);
                }
            }
            "link" => self.toggle_link(),
            "pick" => {
                if let Some(value) = value {
                    let mut parts = value.split(':');
                    let side = parts.next().and_then(side_of).unwrap_or(Side::Source);
                    let x = parts.next().and_then(|part| part.parse::<usize>().ok());
                    let y = parts.next().and_then(|part| part.parse::<usize>().ok());
                    if let (Some(x), Some(y)) = (x, y) {
                        match side {
                            Side::Source => {
                                self.src_ix_field.set_text(x.to_string());
                                self.src_iy_field.set_text(y.to_string());
                            }
                            Side::Result => {
                                self.dst_ix_field.set_text(x.to_string());
                                self.dst_iy_field.set_text(y.to_string());
                            }
                        }
                        self.project.point = Some([x, y]);
                        self.submit_now();
                    }
                }
            }
            "jump" => {
                if let Some(frame) = value.and_then(|part| part.parse::<usize>().ok()) {
                    if let Some((name, _, len)) = self.frame_state_for(Side::Result)
                        && frame < len
                    {
                        self.slice.indices.insert(name, frame);
                        self.project.slice = self.slice.clone();
                        self.submit_now();
                    }
                }
            }
            "dimprev" | "dimnext" => {
                if let Some(name) = value {
                    self.step_dim(name, if act == "dimprev" { -1 } else { 1 });
                }
            }
            "swapxy" => {
                std::mem::swap(&mut self.slice.x, &mut self.slice.y);
                std::mem::swap(&mut self.source_slice.x, &mut self.source_slice.y);
                self.mark_dirty(false);
            }
            "frame-prev" | "frame-next" => {
                let side = value.and_then(side_of).unwrap_or(Side::Result);
                self.frame_step(side, if act == "frame-prev" { -1 } else { 1 });
            }
            "play" => {
                let side = value.and_then(side_of).unwrap_or(Side::Result);
                self.toggle_play(side);
            }
            "op" => {
                if let Some(op) = value {
                    let _ = self.graph_action("graph-add", op);
                }
            }
            "cmap" => {
                if let Some(name) = value {
                    self.colormap = color_map(name);
                    self.needs_render = true;
                }
            }
            "opacity" => {
                if let Some(delta) = value.and_then(|v| v.parse::<f32>().ok()) {
                    self.opacity = (self.opacity + delta).clamp(0.0, 1.0);
                    self.needs_render = true;
                }
            }
            "stride" => {
                if let Some(delta) = value.and_then(|v| v.parse::<i64>().ok()) {
                    self.stride = (self.stride as i64 + delta).clamp(1, 64) as usize;
                    self.needs_render = true;
                }
            }
            "arrowscale" => {
                if let Some(delta) = value.and_then(|v| v.parse::<f32>().ok()) {
                    self.arrow_scale = (self.arrow_scale + delta).clamp(0.1, 4.0);
                    self.needs_render = true;
                }
            }
            "zoom" => {
                match value {
                    Some("reset") => self.zoom = 3,
                    Some("in") => self.zoom = (self.zoom + 1).min(7),
                    Some("out") => self.zoom = (self.zoom - 1).max(1),
                    _ => {}
                }
                self.needs_render = true;
            }
            "pan" => {
                match value {
                    Some("left") => self.pan_x = (self.pan_x - 1).max(-8),
                    Some("right") => self.pan_x = (self.pan_x + 1).min(8),
                    Some("up") => self.pan_y = (self.pan_y - 1).max(-8),
                    Some("down") => self.pan_y = (self.pan_y + 1).min(8),
                    Some("center") => {
                        self.pan_x = 0;
                        self.pan_y = 0;
                    }
                    _ => {}
                }
                self.needs_render = true;
            }
            "export" => match value {
                Some(ext) => self.export_as(ext),
                None => self.export_now(),
            },
            "quit" => self.quit = true,
            _ => {}
        }
        Ok(())
    }

    fn handle_key(&mut self, key: &Key, session: &mut Session<Msg>, sf: Surface) -> Result<bool> {
        if key.is("ctrl+q") {
            return Ok(true);
        }
        if key.is("ctrl+enter") {
            self.project.script = self.script.text.clone();
            self.submit_now();
            return Ok(false);
        }
        if key.is("tab") {
            self.cycle_focus(1, session, sf)?;
            return Ok(false);
        }
        if key.is("shift+tab") {
            self.cycle_focus(-1, session, sf)?;
            return Ok(false);
        }
        if key.is("escape") {
            if self.focused.is_some() {
                self.clear_focus(session, sf)?;
            } else {
                return Ok(true);
            }
            return Ok(false);
        }
        if let Some(name) = self.focused.clone() {
            self.edit_key(&name, key);
            return Ok(false);
        }
        match self.kind {
            Kind::Field => self.field_key(key),
            Kind::Unknown => {}
        }
        Ok(false)
    }

    fn edit_key(&mut self, name: &str, key: &Key) {
        if key.name == "enter" {
            if name == "script" {
                let changed = self.field_mut(name).is_some_and(|field| field.insert("\n"));
                if changed {
                    self.script_edited(true);
                }
            } else {
                self.submit_field(name);
            }
            return;
        }
        if key.name == "paste" {
            let Some(text) = key.text.as_deref() else { return };
            let changed = self.field_mut(name).is_some_and(|field| field.insert(text));
            if changed {
                if name == "script" {
                    self.script_edited(true);
                } else {
                    self.needs_render = true;
                }
            }
            return;
        }
        if let Some(text) = key.typed() {
            let changed = self.field_mut(name).is_some_and(|field| field.insert(text));
            if changed {
                if name == "script" {
                    self.script_edited(true);
                } else {
                    self.needs_render = true;
                }
            }
            return;
        }
        let mut edited = false;
        let mut moved = false;
        match key.name.as_str() {
            "backspace" => {
                edited = self.field_mut(name).is_some_and(|field| field.backspace(key.ctrl || key.alt));
            }
            "delete" => {
                edited = self
                    .field_mut(name)
                    .is_some_and(|field| field.delete_forward(key.ctrl || key.alt));
            }
            "left" => {
                if let Some(field) = self.field_mut(name) {
                    field.left(key.ctrl || key.alt);
                }
                moved = true;
            }
            "right" => {
                if let Some(field) = self.field_mut(name) {
                    field.right(key.ctrl || key.alt);
                }
                moved = true;
            }
            "home" => {
                if let Some(field) = self.field_mut(name) {
                    field.home();
                }
                moved = true;
            }
            "end" => {
                if let Some(field) = self.field_mut(name) {
                    field.end();
                }
                moved = true;
            }
            _ => {}
        }
        if edited {
            if name == "script" {
                self.script_edited(true);
            } else {
                self.needs_render = true;
            }
        } else if moved {
            self.needs_render = true;
        }
    }

    fn field_key(&mut self, key: &Key) {
        match key.name.as_str() {
            "left" => self.pan_x = (self.pan_x - 1).max(-8),
            "right" => self.pan_x = (self.pan_x + 1).min(8),
            "up" => self.pan_y = (self.pan_y - 1).max(-8),
            "down" => self.pan_y = (self.pan_y + 1).min(8),
            "+" | "=" => self.zoom = (self.zoom + 1).min(7),
            "-" | "_" => self.zoom = (self.zoom - 1).max(1),
            "[" => self.frame_step(Side::Result, -1),
            "]" => self.frame_step(Side::Result, 1),
            "space" => self.toggle_play(Side::Result),
            "home" => {
                self.pan_x = 0;
                self.pan_y = 0;
            }
            _ => return,
        }
        self.needs_render = true;
    }

    fn submit_field(&mut self, name: &str) {
        match name {
            "open" => {
                let path = self.open_field.text.trim().to_owned();
                if path.is_empty() {
                    self.status = Notice::err("Type a data file path first.");
                } else {
                    self.set_source(path);
                }
            }
            "project" => {
                let path = self.project_field.text.trim().to_owned();
                if path.is_empty() {
                    self.status = Notice::err("Type a project path first.");
                } else {
                    match self.save_to(&path) {
                        Ok(()) => self.status = Notice::ok(format!("Saved {path}")),
                        Err(err) => self.status = Notice::err(format!("Save project: {err:#}")),
                    }
                }
            }
            "dest" => self.export_now(),
            "script" => {
                self.project.script = self.script.text.clone();
                self.submit_now();
            }
            "src-frame" => self.frame_from_field(Side::Source),
            "dst-frame" => self.frame_from_field(Side::Result),
            "src-min" | "src-max" => self.apply_range(Side::Source),
            "dst-min" | "dst-max" => self.apply_range(Side::Result),
            _ => self.needs_render = true,
        }
    }

    fn frame_from_field(&mut self, side: Side) {
        let text = match side {
            Side::Source => self.src_frame_field.text.trim().to_owned(),
            Side::Result => self.dst_frame_field.text.trim().to_owned(),
        };
        let Ok(value) = text.parse::<usize>() else {
            self.status = Notice::err("Enter a 1-based frame number.");
            return;
        };
        if let Some((name, _, len)) = self.frame_state_for(side) {
            if len == 0 {
                return;
            }
            let index = value.saturating_sub(1).min(len - 1);
            self.set_frame(side, name, index);
            self.submit_now();
        }
    }

    fn focus_key(&mut self, key: &str, session: &mut Session<Msg>, sf: Surface) -> Result<()> {
        self.focused = Some(key.to_owned());
        let view = self.view();
        if let Some(id) = find_field_id(&view, key) {
            session.focus(sf, Some(id.as_str()))?;
            self.focused_id = Some(id);
        } else {
            self.focused = None;
            session.focus(sf, None)?;
            self.focused_id = None;
        }
        self.needs_render = true;
        Ok(())
    }

    fn clear_focus(&mut self, session: &mut Session<Msg>, sf: Surface) -> Result<()> {
        self.focused = None;
        if self.focused_id.take().is_some() {
            session.focus(sf, None)?;
        }
        Ok(())
    }

    fn cycle_focus(&mut self, step: i32, session: &mut Session<Msg>, sf: Surface) -> Result<()> {
        let mut keys = vec!["open", "project"];
        if self.kind == Kind::Field {
            keys.extend(["dest", "script"]);
            keys.extend(["src-ix", "src-iy", "dst-ix", "dst-iy", "src-min", "src-max", "dst-min", "dst-max"]);
            if self.frame_state_for(Side::Source).is_some() {
                keys.push("src-frame");
            }
            if self.frame_state_for(Side::Result).is_some() {
                keys.push("dst-frame");
            }
        }
        let current = self
            .focused
            .as_deref()
            .and_then(|name| keys.iter().position(|key| *key == name));
        let next = match current {
            Some(index) => ((index as i32 + step).rem_euclid(keys.len() as i32)) as usize,
            None => {
                if step >= 0 {
                    0
                } else {
                    keys.len() - 1
                }
            }
        };
        let key = keys[next].to_owned();
        self.focus_key(&key, session, sf)
    }

    fn mark_dirty(&mut self, debounce: bool) {
        self.dirty = true;
        let at = if debounce { Instant::now() + DEBOUNCE } else { Instant::now() };
        self.dirty_at = Some(self.dirty_at.map_or(at, |old| old.min(at)));
        self.needs_render = true;
    }

    fn script_edited(&mut self, debounce: bool) {
        self.project.script = self.script.text.clone();
        self.project.result_variable.clear();
        self.mark_dirty(debounce);
    }

    fn submit_now(&mut self) {
        self.dirty = false;
        self.dirty_at = None;
        if self.kind != Kind::Field || self.project.path.trim().is_empty() {
            self.status = Notice::err("Open a NetCDF file first.");
            self.needs_render = true;
            return;
        }
        self.project.script = self.script.text.clone();
        self.project.slice = self.slice.clone();
        self.project.source_slice = if self.linked { None } else { Some(self.source_slice.clone()) };
        let generation = self.worker.submit(self.project.clone());
        self.newest_data_gen = self.newest_data_gen.max(generation);
        self.status = Notice::busy("Rendering…");
        self.needs_render = true;
    }

    fn apply_completion(&mut self, completion: Completion, session: &mut Session<Msg>) -> Result<()> {
        if Some(completion.generation) == self.export_pending {
            self.export_pending = None;
            match completion.result {
                Ok(Output::Exported(path)) => {
                    self.status = Notice::ok(format!("Exported {path}"));
                }
                Ok(_) => self.status = Notice::err("Export returned an unexpected result."),
                Err(err) => self.status = Notice::err(format!("Export failed: {err:#}")),
            }
            self.needs_render = true;
            return Ok(());
        }
        if completion.generation < self.newest_data_gen {
            return Ok(());
        }
        self.newest_data_gen = completion.generation;
        self.last_ms = completion.elapsed_ms;
        match completion.result {
            Ok(Output::Field {
                info,
                slice,
                source_slice,
                source_variable,
                results,
                source,
                rendered,
                series,
            }) => {
                self.kind = Kind::Field;
                self.info = Some(info);
                if !self.dirty {
                    let stale = !self.project.result_variable.is_empty()
                        && !results
                            .iter()
                            .any(|result| result.name == self.project.result_variable);
                    self.project.source_variable = source_variable;
                    self.results = results;
                    self.slice = slice;
                    self.source_slice = source_slice;
                    if stale {
                        self.project.result_variable.clear();
                    }
                }
                let source_blob = match blob_for(&source, self.source_view.as_ref(), session) {
                    Ok(blob) => blob,
                    Err(err) => {
                        self.status = Notice::err(format!("Source image: {err}"));
                        None
                    }
                };
                let result_blob = match blob_for(&rendered, self.result_view.as_ref(), session) {
                    Ok(blob) => blob,
                    Err(err) => {
                        self.status = Notice::err(format!("Result image: {err}"));
                        None
                    }
                };
                let (source_width, source_height) = (source.width, source.height);
                let (result_width, result_height) = (rendered.width, rendered.height);
                self.source_view = Some(PaneView { rendered: source, blob: source_blob });
                self.result_view = Some(PaneView { rendered, blob: result_blob });
                self.series = series;
                self.ensure_field_source();
                if !self.dirty {
                    self.sync_frame_fields();
                }
                if self.status.busy {
                    self.status = Notice::ok(format!(
                        "{}×{} → {}×{} · {:.0} ms",
                        source_width, source_height, result_width, result_height, self.last_ms
                    ));
                }
                self.autosave();
            }
            Ok(Output::Exported(path)) => {
                self.status = Notice::ok(format!("Exported {path}"));
            }
            Err(err) => {
                self.status = Notice::err(format!("{err:#}"));
            }
        }
        self.needs_render = true;
        Ok(())
    }

    fn ensure_field_source(&mut self) {
        let Some(info) = &self.info else { return };
        if self.project.source_variable.is_empty() {
            if let Some(first) = info.variables.first() {
                self.project.source_variable = first.name.clone();
            }
        }
        let name = self.project.source_variable.clone();
        if name.is_empty() {
            return;
        }
        if self.slot_a.is_none() {
            self.slot_a = Some(name.clone());
        }
        if self.script.text.trim().is_empty() {
            self.script = Field::new(graph::default_script(&name));
            self.project.script = self.script.text.clone();
        }
        if self.project.graph.is_none() {
            self.project.graph = Some(NodeGraph::source(&name));
            self.selected_node = Some(1);
        }
    }

    fn set_source(&mut self, path: String) {
        let path = path.trim().to_owned();
        if !is_field(&path) {
            self.status =
                Notice::err("This surface opens NetCDF files.");
            self.needs_render = true;
            return;
        }
        self.project = Project {
            path: path.clone(),
            ..Project::default()
        };
        self.open_field = Field::new(path);
        self.kind = Kind::Field;
        self.script = Field::new("");
        self.slice = Slice::default();
        self.source_slice = Slice::default();
        self.linked = true;
        self.info = None;
        self.source_view = None;
        self.result_view = None;
        self.results.clear();
        self.series = None;
        self.slot_a = None;
        self.slot_b = None;
        self.selected_node = None;
        self.connect_from = None;
        self.playing = None;
        self.newest_data_gen = 0;
        self.export_pending = None;
        self.dirty = false;
        self.dirty_at = None;
        self.src_frame_field = Field::new("1");
        self.dst_frame_field = Field::new("1");
        self.src_ix_field = Field::new("0");
        self.src_iy_field = Field::new("0");
        self.dst_ix_field = Field::new("0");
        self.dst_iy_field = Field::new("0");
        self.src_min_field = Field::new("");
        self.src_max_field = Field::new("");
        self.dst_min_field = Field::new("");
        self.dst_max_field = Field::new("");
        self.status = Notice::busy(format!("Opening {}…", self.project.path));
        self.dest_field = Field::new(self.default_dest("png"));
        self.submit_now();
        self.needs_render = true;
    }

    fn default_dest(&self, ext: &str) -> String {
        let path = self.project.path.trim();
        if path.is_empty() {
            return format!("netcdf.{ext}");
        }
        Path::new(path)
            .with_extension(ext)
            .to_string_lossy()
            .into_owned()
    }

    fn export_as(&mut self, ext: &str) {
        let current = self.dest_field.text.trim();
        let dest = if current.is_empty() {
            self.default_dest(ext)
        } else {
            Path::new(current).with_extension(ext).to_string_lossy().into_owned()
        };
        self.dest_field = Field::new(dest.clone());
        self.export_to(dest);
    }

    fn export_now(&mut self) {
        let dest = self.dest_field.text.trim().to_owned();
        if dest.is_empty() {
            self.status = Notice::err("Enter an export path first.");
            return;
        }
        self.export_to(dest);
    }

    fn export_to(&mut self, dest: String) {
        if self.export_pending.is_some() {
            self.status = Notice::err("An export is already running.");
            return;
        }
        if self.kind != Kind::Field || self.project.path.trim().is_empty() {
            self.status = Notice::err("Open a NetCDF file first.");
            return;
        }
        self.project.script = self.script.text.clone();
        self.project.slice = self.slice.clone();
        self.project.source_slice = if self.linked { None } else { Some(self.source_slice.clone()) };
        if Path::new(&dest).extension().is_some_and(|ext| ext == "py") {
            let result = (|| -> Result<()> {
                use std::io::Write;
                let script = crate::python::export(&self.project)?;
                let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&dest)?;
                file.write_all(script.as_bytes())?;
                Ok(())
            })();
            self.status = match result { Ok(()) => Notice::ok(format!("Exported {dest}")), Err(err) => Notice::err(format!("Python export: {err:#}")) };
            self.needs_render = true;
            return;
        }
        let generation = self.worker.export(self.project.clone(), dest.clone());
        self.export_pending = Some(generation);
        self.status = Notice::busy(format!("Exporting {dest}…"));
        self.needs_render = true;
    }

    fn save_to(&mut self, path: &str) -> Result<()> {
        self.write_project(path)?;
        self.project_path = Some(PathBuf::from(path));
        Ok(())
    }

    fn autosave(&mut self) {
        let Some(path) = self.project_path.clone() else { return };
        if let Err(err) = self.write_project(&path.to_string_lossy()) {
            self.status = Notice::err(format!("Autosave: {err:#}"));
        }
    }

    fn write_project(&mut self, path: &str) -> Result<()> {
        self.project.script = self.script.text.clone();
        self.project.slice = self.slice.clone();
        self.project.source_slice = if self.linked { None } else { Some(self.source_slice.clone()) };
        let bytes = serde_json::to_vec_pretty(&self.project)?;
        let target = PathBuf::from(path);
        let dir = target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        fs::create_dir_all(&dir).with_context(|| format!("Create {}", dir.display()))?;
        let name = target.file_name().and_then(|name| name.to_str()).unwrap_or("project.json");
        let temp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
        fs::write(&temp, &bytes).with_context(|| format!("Write {}", temp.display()))?;
        if let Err(err) = fs::rename(&temp, &target) {
            let _ = fs::remove_file(&temp);
            return Err(err).with_context(|| format!("Replace {}", target.display()));
        }
        Ok(())
    }

    fn load_from(&mut self, path: &str) -> Result<()> {
        let text = fs::read_to_string(path).with_context(|| format!("Read {path}"))?;
        let project: Project = serde_json::from_str(&text).context("Parse project JSON")?;
        self.project = project;
        self.script = Field::new(self.project.script.clone());
        self.slice = self.project.slice.clone();
        self.linked = self.project.source_slice.is_none();
        self.source_slice = self
            .project
            .source_slice
            .clone()
            .unwrap_or_else(|| self.project.slice.clone());
        self.sync_range_fields();
        if let Some([x, y]) = self.project.point {
            self.src_ix_field = Field::new(x.to_string());
            self.src_iy_field = Field::new(y.to_string());
            self.dst_ix_field = Field::new(x.to_string());
            self.dst_iy_field = Field::new(y.to_string());
        }
        self.info = None;
        self.source_view = None;
        self.result_view = None;
        self.results.clear();
        self.series = None;
        self.playing = None;
        self.kind = if is_field(&self.project.path) { Kind::Field } else { Kind::Unknown };
        self.slot_a = scan_field_var(&self.project.script, "a");
        self.slot_b = scan_field_var(&self.project.script, "b");
        if self.slot_a.is_none() && !self.project.source_variable.is_empty() {
            self.slot_a = Some(self.project.source_variable.clone());
        }
        self.project_path = Some(PathBuf::from(path));
        self.project_field = Field::new(path);
        self.open_field = Field::new(self.project.path.clone());
        if self.dest_field.text.trim().is_empty() {
            self.dest_field = Field::new(self.default_dest("png"));
        }
        self.newest_data_gen = 0;
        self.export_pending = None;
        self.dirty = false;
        self.dirty_at = None;
        self.status = Notice::busy(format!("Loading {path}…"));
        if self.kind == Kind::Field {
            self.submit_now();
        } else if !self.project.path.trim().is_empty() {
            self.status =
                Notice::err("This surface opens NetCDF files.");
        } else {
            self.status = Notice::ok("Loaded project; enter a NetCDF path.");
        }
        self.needs_render = true;
        Ok(())
    }

    fn source_name(&self) -> Option<&str> {
        (!self.project.source_variable.is_empty()).then_some(self.project.source_variable.as_str())
    }

    fn current_variable<'a>(&self, info: &'a DatasetInfo) -> Option<&'a crate::model::Variable> {
        let name = self.source_name()?;
        info.variables.iter().find(|variable| variable.name == name)
    }

    fn frame_state_for(&self, side: Side) -> Option<(String, usize, usize)> {
        let info = self.info.as_ref()?;
        let variable = self.current_variable(info)?;
        let slice = match side {
            Side::Source => &self.source_slice,
            Side::Result => &self.slice,
        };
        if slice.x.is_empty() || slice.y.is_empty() {
            return None;
        }
        let dim = variable
            .dimensions
            .iter()
            .filter(|dim| dim.name != slice.x && dim.name != slice.y)
            .min_by_key(|dim| if dim.name.to_ascii_lowercase().contains("time") { 0 } else { 1 })?;
        let index = slice
            .indices
            .get(&dim.name)
            .copied()
            .unwrap_or(0)
            .min(dim.len.saturating_sub(1));
        Some((dim.name.clone(), index, dim.len))
    }

    fn frame_step(&mut self, side: Side, delta: i64) {
        let Some((name, index, len)) = self.frame_state_for(side) else {
            self.status = Notice::err("This field has no frame dimension.");
            self.needs_render = true;
            return;
        };
        if len <= 1 {
            return;
        }
        let next = (index as i64 + delta).rem_euclid(len as i64) as usize;
        self.set_frame(side, name, next);
        self.submit_now();
    }

    fn set_frame(&mut self, side: Side, name: String, index: usize) {
        match side {
            Side::Source => {
                self.source_slice.indices.insert(name.clone(), index);
                if self.linked {
                    self.slice.indices.insert(name, index);
                }
            }
            Side::Result => {
                self.slice.indices.insert(name.clone(), index);
                if self.linked {
                    self.source_slice.indices.insert(name, index);
                }
            }
        }
        self.project.slice = self.slice.clone();
        self.project.source_slice = if self.linked { None } else { Some(self.source_slice.clone()) };
        let source_frame = self
            .frame_state_for(Side::Source)
            .map(|(_, index, _)| index + 1)
            .unwrap_or(index + 1);
        let result_frame = self
            .frame_state_for(Side::Result)
            .map(|(_, index, _)| index + 1)
            .unwrap_or(index + 1);
        self.src_frame_field = Field::new(source_frame.to_string());
        self.dst_frame_field = Field::new(result_frame.to_string());
    }

    fn toggle_play(&mut self, side: Side) {
        if self.playing == Some(side) {
            self.playing = None;
        } else {
            let count = self.frame_state_for(side).map(|(_, _, len)| len).unwrap_or(0);
            if count > 1 {
                self.playing = Some(side);
                self.play_due = Instant::now();
            } else {
                self.status = Notice::err("No frame dimension to play.");
            }
        }
        self.needs_render = true;
    }

    fn step_dim(&mut self, name: &str, delta: i64) {
        let Some(info) = &self.info else { return };
        let Some(variable) = self.current_variable(info) else { return };
        let Some(dim) = variable.dimensions.iter().find(|dim| dim.name == name) else { return };
        if name == self.slice.x || name == self.slice.y || dim.len == 0 {
            return;
        }
        let index = self.slice.indices.get(name).copied().unwrap_or(0).min(dim.len - 1);
        let next = (index as i64 + delta).rem_euclid(dim.len as i64) as usize;
        self.slice.indices.insert(name.to_owned(), next);
        self.source_slice.indices.insert(name.to_owned(), next);
        self.mark_dirty(false);
    }

    fn choose_source(&mut self, name: &str) {
        let same_dims = self
            .info
            .as_ref()
            .and_then(|info| info.variables.iter().find(|variable| variable.name == name))
            .is_some_and(|variable| {
                variable.dimensions.iter().any(|dim| dim.name == self.slice.x)
                    && variable.dimensions.iter().any(|dim| dim.name == self.slice.y)
            });
        self.project.source_variable = name.to_owned();
        self.project.source_slice = None;
        self.linked = true;
        self.source_slice = Slice::default();
        if !same_dims {
            self.slice = Slice::default();
            self.project.slice = Slice::default();
        }
        self.slot_a = Some(name.to_owned());
        if self.script.text.trim().is_empty() {
            self.script = Field::new(graph::default_script(name));
            self.project.script = self.script.text.clone();
        }
        self.mark_dirty(false);
    }

    fn choose_result(&mut self, name: &str) {
        self.project.result_variable = name.to_owned();
        self.mark_dirty(false);
    }

    fn graph_action(&mut self, act: &str, value: &str) -> Result<()> {
        use anyhow::{Context, ensure};
        let parts: Vec<&str> = value.split(':').collect();
        if act == "graph-select" {
            self.selected_node = Some(value.parse()?);
            return Ok(());
        }
        if act == "graph-output" {
            self.connect_from = Some(value.parse()?);
            self.status = Notice::ok("Select an input port to connect this output.");
            return Ok(());
        }
        let graph = self.project.graph.as_mut().context("Open a dataset first")?;
        let previous = graph.clone();
        match act {
            "graph-add" => {
                let id = graph.nodes.iter().map(|n| n.id).max().unwrap_or(0) + 1;
                let (operation, count, title) = match value {
                    "gaussian" => (Operation::Gaussian { sigma: 1.2 }, 1, "Gaussian".to_owned()),
                    "convolve" => (Operation::Convolve { kernel: vec![1.0 / 9.0; 9], side: 3 }, 1, "Convolve".into()),
                    "difference" => (Operation::Difference, 2, "Difference".into()),
                    "addition" => (Operation::Addition, 2, "Addition".into()),
                    "filter" => (Operation::Filter { min: Some(0.0), max: Some(1.0) }, 1, "Filter".into()),
                    "median" => (Operation::Median { radius: 1 }, 1, "Median".into()),
                    "fft2" => (Operation::Fft2, 1, "FFT2".into()),
                    "ifft2" => (Operation::Ifft2, 1, "IFFT2".into()),
                    "scale" => (Operation::Scale { factor: 1.0, offset: 0.0 }, 1, "Scale".into()),
                    "gradient" => (Operation::Gradient, 1, "Gradient".into()),
                    "vector" => (Operation::Vector, 2, "Vector".into()),
                    "magnitude" => (Operation::Magnitude, 1, "Magnitude".into()),
                    "time_shift" => (Operation::TimeShift { steps: 1 }, 1, "t+n".into()),
                    _ => (Operation::Source { variable: value.into() }, 0, value.into()),
                };
                let mut inputs = vec![None; count];
                if count > 0 { inputs[0] = self.selected_node; }
                graph.nodes.push(GraphNode { id, name: title, position: [40.0 + ((id - 1) % 4) as f64 * 240.0, 40.0 + ((id - 1) / 4) as f64 * 200.0], inputs, operation });
                self.selected_node = Some(id);
            },
            "graph-input" => {
                let id: u64 = parts[0].parse()?;
                let slot: usize = parts.get(1).context("Missing port")?.parse()?;
                let from = self.connect_from.take().context("Select an output port first")?;
                ensure!(id != from, "A node cannot connect to itself");
                let node = graph.nodes.iter_mut().find(|n| n.id == id).context("Missing node")?;
                *node.inputs.get_mut(slot).context("Missing input")? = Some(from);
                if let Err(err) = graph.compile() { *graph = previous; return Err(err); }
            },
            "graph-remove" => {
                let id: u64 = value.parse()?;
                graph.nodes.retain(|n| n.id != id);
                graph.layers.retain(|l| l.node != id);
                for n in &mut graph.nodes { for slot in &mut n.inputs { if *slot == Some(id) { *slot = None; } } }
                self.selected_node = graph.nodes.first().map(|n| n.id);
            },
            "layer-add" => {
                let id: u64 = value.parse()?;
                if !graph.layers.iter().any(|l| l.node == id) {
                    graph.layers.push(GraphLayer { node: id, visible: true, colormap: ColorMap::Viridis, opacity: if graph.layers.is_empty() { 1.0 } else { 0.5 }, range: None, stride: 8, arrow_scale: 1.0 });
                }
            },
            "layer-remove" => {
                let id: u64 = value.parse()?;
                graph.layers.retain(|l| l.node != id);
            },
            "graph-param" | "graph-move" => {
                let id: u64 = parts[0].parse()?;
                let key = *parts.get(1).context("Missing parameter")?;
                let delta: f64 = parts.get(2).context("Missing value")?.parse()?;
                let node = graph.nodes.iter_mut().find(|n| n.id == id).context("Missing node")?;
                if act == "graph-move" { node.position[if key == "x" { 0 } else { 1 }] += delta; }
                else { match &mut node.operation {
                    Operation::Gaussian { sigma } => *sigma = (*sigma + delta).clamp(0.1, 64.0),
                    Operation::Scale { factor, offset } => if key == "factor" { *factor += delta } else { *offset += delta },
                    Operation::TimeShift { steps } => if let Some(next) = steps.checked_add(delta as i64) { *steps = next },
                    Operation::Filter { min, max } => {
                        if key == "min" {
                            if let Some(m) = min { *m += delta; } else { *min = Some(delta); }
                        } else if key == "max" {
                            if let Some(m) = max { *m += delta; } else { *max = Some(delta); }
                        }
                    },
                    Operation::Median { radius } => {
                        *radius = (*radius as isize + delta as isize).clamp(1, 15) as usize;
                    },
                    _ => {},
                } }
            },
            "layer-order" => {
                let id: u64 = parts[0].parse()?;
                let delta: isize = parts.get(1).context("Missing direction")?.parse()?;
                let i = graph.layers.iter().position(|l| l.node == id).context("Missing layer")?;
                let j = (i as isize + delta).clamp(0, graph.layers.len() as isize - 1) as usize;
                graph.layers.swap(i,j);
            },
            _ => {
                let id: u64 = parts[0].parse()?;
                let layer = graph.layers.iter_mut().find(|l| l.node == id).context("Missing layer")?;
                match act {
                    "layer-toggle" => layer.visible = !layer.visible,
                    "layer-map" => layer.colormap = color_map(parts.get(1).context("Missing palette")?),
                    "layer-opacity" => layer.opacity = parts.get(1).context("Missing opacity")?.parse::<f32>()?.clamp(0.0,1.0),
                    _ => {},
                }
            }
        }
        self.project.result_variable.clear();
        self.autosave();
        self.mark_dirty(true);
        Ok(())
    }

    fn choose_second(&mut self, name: &str) {
        self.slot_b = Some(name.to_owned());
        let _ = self.graph_action("graph-add", name);
    }

    fn toggle_link(&mut self) {
        if self.linked {
            if self.source_slice.x.is_empty() || self.source_slice.y.is_empty() {
                self.source_slice = self.slice.clone();
            }
            self.project.source_slice = Some(self.source_slice.clone());
            self.linked = false;
        } else {
            self.project.source_slice = None;
            self.linked = true;
            self.source_slice = self.slice.clone();
        }
        self.mark_dirty(false);
    }

    fn apply_range(&mut self, side: Side) {
        let (min_text, max_text) = match side {
            Side::Source => (self.src_min_field.text.trim().to_owned(), self.src_max_field.text.trim().to_owned()),
            Side::Result => (self.dst_min_field.text.trim().to_owned(), self.dst_max_field.text.trim().to_owned()),
        };
        let range = if min_text.is_empty() && max_text.is_empty() {
            None
        } else {
            let (Ok(low), Ok(high)) = (min_text.parse::<f32>(), max_text.parse::<f32>()) else {
                self.status = Notice::err("A fixed range needs two numbers.");
                return;
            };
            if !low.is_finite() || !high.is_finite() || low >= high {
                self.status = Notice::err("Range needs finite bounds with low < high.");
                return;
            }
            Some([low, high])
        };
        match side {
            Side::Source => self.project.source_range = range,
            Side::Result => self.project.result_range = range,
        }
        self.sync_range_fields();
        self.mark_dirty(false);
    }

    fn sync_range_fields(&mut self) {
        let (src_low, src_high) = range_text(self.project.source_range);
        let (dst_low, dst_high) = range_text(self.project.result_range);
        self.src_min_field = Field::new(src_low);
        self.src_max_field = Field::new(src_high);
        self.dst_min_field = Field::new(dst_low);
        self.dst_max_field = Field::new(dst_high);
    }

    fn sync_frame_fields(&mut self) {
        if self.focused.as_deref() != Some("src-frame") {
            if let Some((_, index, _)) = self.frame_state_for(Side::Source) {
                self.src_frame_field = Field::new((index + 1).to_string());
            }
        }
        if self.focused.as_deref() != Some("dst-frame") {
            if let Some((_, index, _)) = self.frame_state_for(Side::Result) {
                self.dst_frame_field = Field::new((index + 1).to_string());
            }
        }
    }

    fn pane(&self, side: Side) -> Option<&PaneView> {
        match side {
            Side::Source => self.source_view.as_ref(),
            Side::Result => self.result_view.as_ref(),
        }
    }

    fn readout(&self, side: Side) -> Option<(usize, usize, f32, Option<f64>, Option<f64>)> {
        let pane = self.pane(side)?;
        let (x, y) = match side {
            Side::Source => (
                self.src_ix_field.text.trim().parse::<usize>().ok()?,
                self.src_iy_field.text.trim().parse::<usize>().ok()?,
            ),
            Side::Result => (
                self.dst_ix_field.text.trim().parse::<usize>().ok()?,
                self.dst_iy_field.text.trim().parse::<usize>().ok()?,
            ),
        };
        let grid = &pane.rendered.grid;
        if x >= grid.width || y >= grid.height {
            return None;
        }
        let value = grid.values.get(y * grid.width + x).copied()?;
        Some((
            x,
            y,
            value,
            pane.rendered.x.values.get(x).copied(),
            pane.rendered.y.values.get(y).copied(),
        ))
    }

    fn readout_text(&self, side: Side) -> String {
        let Some(pane) = self.pane(side) else {
            return "No image yet.".to_owned();
        };
        let (x_text, y_text) = match side {
            Side::Source => (&self.src_ix_field.text, &self.src_iy_field.text),
            Side::Result => (&self.dst_ix_field.text, &self.dst_iy_field.text),
        };
        let (Ok(x), Ok(y)) = (x_text.trim().parse::<usize>(), y_text.trim().parse::<usize>()) else {
            return format!(
                "Indices 0..{} × 0..{}",
                pane.rendered.grid.width.saturating_sub(1),
                pane.rendered.grid.height.saturating_sub(1)
            );
        };
        if x >= pane.rendered.grid.width || y >= pane.rendered.grid.height {
            return format!(
                "Outside 0..{} × 0..{}",
                pane.rendered.grid.width.saturating_sub(1),
                pane.rendered.grid.height.saturating_sub(1)
            );
        }
        match self.readout(side) {
            Some((x, y, value, xc, yc)) => {
                let coord = match (xc, yc) {
                    (Some(xc), Some(yc)) => format!(" at ({}, {})", fmt_f64(xc), fmt_f64(yc)),
                    _ => String::new(),
                };
                format!("x={x} y={y} → {}{coord} {}", fmt_f32(value), pane.rendered.units)
            }
            None => "No value.".to_owned(),
        }
    }

    fn readout_value(&self, side: Side) -> Option<String> {
        let (_, _, value, _, _) = self.readout(side)?;
        Some(fmt_f32(value))
    }

    // ----------------------------------------------------------------- views

    fn view(&self) -> View<Msg> {
        let body = match self.kind {
            Kind::Field => self.field_body(),
            Kind::Unknown => self.empty_body(),
        };
        let root: Node<Msg> = ui::col::<Msg>()
            .child(
                html::div::<Msg>()
                    .class("db-app")
                    .child(self.header())
                    .child(body),
            )
            .into();
        View::new().main(root)
    }

    fn header(&self) -> Node<Msg> {
        let open = self.field_input(&self.open_field, "open", "Data path (.nc, .nc4, .cdf, .mat)", true);
        let project = self.field_input(&self.project_field, "project", "Project JSON path", true);
        html::div::<Msg>()
            .class("db-head")
            .child(
                div("db-row")
                    .child(div("db-brand").child(ui::text::<Msg>("NetCDF")))
                    .child(open)
                    .child(btn("Open", "open", None, "db-btn primary"))
                    .child(btn("Run ^Enter", "run", None, "db-btn primary")),
            )
            .child(
                div("db-row")
                    .child(ui::text::<Msg>("Project"))
                    .child(project)
                    .child(btn("Save", "save", None, "db-btn"))
                    .child(btn("Load", "load", None, "db-btn"))
                    .child(chip("mode", self.kind.label()))
                    .child(chip("last", format!("{:.0} ms", self.last_ms))),
            )
            .child(
                div("db-row")
                    .child(div("db-path").child(ui::text::<Msg>(self.project.path.clone())))
                    .child(
                        div(if self.status.ok { "db-detail" } else { "db-detail db-err" })
                            .child(ui::text::<Msg>(self.status.text.clone())),
                    ),
            )
            .into()
    }

    fn empty_body(&self) -> Node<Msg> {
        let mut panel = div("db-panel")
            .child(div("db-panel-h").child(ui::text::<Msg>("Start")))
            .child(hint("Type a data path (`.nc`, `.nc4`, `.cdf`, `.mat`) above and press Open."))
            .child(hint("Project JSON is optional. When a --project path was supplied, the first successful result is saved there."));
        if !self.project_field.text.trim().is_empty() {
            panel = panel.child(hint(format!("Project: {}", self.project_field.text)));
        }
        div("db-main-col").child(panel).into()
    }

    fn field_body(&self) -> Node<Msg> {
        div("db-workspace")
            .child(
                div("db-stage")
                    .child(self.source_rail())
                    .child(self.pane_view(Side::Source))
                    .child(self.pane_view(Side::Result))
                    .child(self.result_rail()),
            )
            .child(div("db-bottom").child(self.graph_library()).child(self.graph_canvas()))
            .into()
    }

    fn source_rail(&self) -> Node<Msg> {
        let mut items = Vec::new();
        if let Some(info) = &self.info {
            for variable in &info.variables {
                let on = self.project.source_variable == variable.name;
                let card = clickable(
                    div(if on { "db-var-card on" } else { "db-var-card" }),
                    "source",
                    Some(&variable.name),
                )
                .child(
                    div("db-var-row-top")
                        .child(html::span::<Msg>(&variable.name).class("db-var-name"))
                        .child(btn("+ Node", "graph-add", Some(&variable.name), "db-mini db-add-btn")),
                )
                .child(
                    div("db-var-tags")
                        .children(
                            variable.dimensions.iter().map(|dim| {
                                div("db-var-tag")
                                    .child(html::span::<Msg>(&dim.name).class("db-tag-name"))
                                    .child(html::span::<Msg>(dim.len.to_string()).class("db-tag-val"))
                            }),
                        )
                        .child(
                            div("db-var-tag unit")
                                .child(html::span::<Msg>(if variable.units.is_empty() { "no unit".to_owned() } else { variable.units.clone() }).class("db-tag-val")),
                        ),
                );
                items.push(card);
            }
        }
        let list: Node<Msg> = if items.is_empty() {
            hint("Open a NetCDF or MAT file to list its numeric variables.")
        } else {
            div("db-side-list").children(items).into()
        };
        div("db-side")
            .child(div("db-side-h").child(ui::text::<Msg>("Source variables")))
            .child(list)
            .child(hint("Click to preview. Click '+ Node' to add to graph."))
            .into()
    }

    fn result_rail(&self) -> Node<Msg> {
        self.layer_stack()
    }

    fn pane_view(&self, side: Side) -> Node<Msg> {
        let title = match side {
            Side::Source => "Before pipeline",
            Side::Result => "After pipeline",
        };
        let name = match side {
            Side::Source => {
                if self.project.source_variable.is_empty() {
                    "source".to_owned()
                } else {
                    self.project.source_variable.clone()
                }
            }
            Side::Result => {
                if self.project.result_variable.is_empty() {
                    "composite".to_owned()
                } else {
                    self.project.result_variable.clone()
                }
            }
        };
        let link = if side == Side::Source {
            if self.linked {
                "linked slice"
            } else {
                "independent slice"
            }
        } else if self.linked {
            "follows source"
        } else {
            "independent slice"
        };
        div("db-view")
            .child(
                div("db-view-h")
                    .child(ui::text::<Msg>(title))
                    .child(chip("field", name))
                    .child(chip("slice", link)),
            )
            .child(div("db-view-canvas").child(self.pane_canvas(side)))
            .child(self.series_node(side))
            .child(self.pane_controls(side))
            .into()
    }

    /// A 12 x 12 lattice over the picture: Tern gives pointer clicks to image
    /// children, so a cell names a grid position without any coordinates.
    fn pick_lattice(&self, side: Side) -> Node<Msg> {
        let mut grid = div("db-picks");
        let Some(pane) = self.pane(side) else {
            return grid.into();
        };
        let (width, height) = (pane.rendered.grid.width, pane.rendered.grid.height);
        for row in 0..12usize {
            for col in 0..12usize {
                let x = (col * width + width / 24) / 12;
                let y = (row * height + height / 24) / 12;
                let value = format!("{}:{x}:{y}", side.key());
                grid = grid.child(clickable(html::el::<Msg>("div").class("db-pick"), "pick", Some(&value)));
            }
        }
        grid.into()
    }

    fn series_node(&self, side: Side) -> Node<Msg> {
        if side != Side::Source {
            return div("db-series-empty").into();
        }
        let Some(series) = &self.series else {
            return div("db-series-empty")
                .child(hint("Click a cell in the picture to read its series over time."))
                .into();
        };
        const BINS: usize = 64;
        let len = series.values.len().max(1);
        let mut sums = vec![(0.0f64, 0usize); BINS];
        for (index, value) in series.values.iter().enumerate() {
            let bin = (index * BINS / len).min(BINS - 1);
            if let Some(value) = value.filter(|value| value.is_finite()) {
                sums[bin].0 += value;
                sums[bin].1 += 1;
            }
        }
        let averages: Vec<Option<f64>> = sums
            .iter()
            .map(|(sum, count)| (*count > 0).then(|| sum / *count as f64))
            .collect();
        let lo = averages.iter().flatten().copied().fold(f64::INFINITY, f64::min);
        let hi = averages.iter().flatten().copied().fold(f64::NEG_INFINITY, f64::max);
        let span = if hi > lo { hi - lo } else { 1.0 };
        let levels = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let mut strip = div("db-series");
        for (bin, average) in averages.iter().enumerate() {
            let level = average
                .map(|value| (((value - lo) / span) * 7.0).round().clamp(0.0, 7.0) as usize)
                .unwrap_or(0);
            let frame = bin * len / BINS;
            let value = frame.to_string();
            strip = strip.child(clickable(
                html::el::<Msg>("div")
                    .class("db-series-cell")
                    .child(ui::text::<Msg>(levels[level].to_string())),
                "jump",
                Some(&value),
            ));
        }
        div("db-series-wrap")
            .child(
                div("db-series-h")
                    .child(ui::text::<Msg>(format!("{} · {} frames", series.name, len)))
                    .child(chip("range", format!("{} … {}", fmt_f64(lo), fmt_f64(hi)))),
            )
            .child(strip)
            .into()
    }

    fn pane_canvas(&self, side: Side) -> Node<Msg> {
        let inner: Node<Msg> = match self.pane(side) {
            Some(pane) => match &pane.blob {
                Some(blob) => {
                    let mut image: Node<Msg> = ui::image::<Msg>()
                        .blob(blob.clone())
                        .alt("rendered field")
                        .w(pane.rendered.width as f64)
                        .h(pane.rendered.height as f64)
                        .actions(Actions::new().click("zoom"))
                        .into();
                    image = image.child(self.pick_lattice(side));
                    div(&format!("db-pan-x {}", pan_x_class(self.pan_x)))
                        .child(
                            div(&format!("db-pan-y {}", pan_y_class(self.pan_y))).child(
                                div(&format!("db-zoom {}", zoom_class(self.zoom))).child(image),
                            ),
                        )
                        .into()
                }
                None => div("db-wait")
                    .child(ui::text::<Msg>("Image unavailable; export a PNG instead."))
                    .into(),
            },
            None => div("db-wait")
                .child(ui::text::<Msg>("No image yet."))
                .into(),
        };
        div("db-canvas").child(inner).into()
    }

    fn pane_controls(&self, side: Side) -> Node<Msg> {
        let (ix_field, iy_field, frame_field, min_field, max_field) = match side {
            Side::Source => (
                &self.src_ix_field,
                &self.src_iy_field,
                &self.src_frame_field,
                &self.src_min_field,
                &self.src_max_field,
            ),
            Side::Result => (
                &self.dst_ix_field,
                &self.dst_iy_field,
                &self.dst_frame_field,
                &self.dst_min_field,
                &self.dst_max_field,
            ),
        };
        let (ix_key, iy_key, frame_key, min_key, max_key) = match side {
            Side::Source => ("src-ix", "src-iy", "src-frame", "src-min", "src-max"),
            Side::Result => ("dst-ix", "dst-iy", "dst-frame", "dst-min", "dst-max"),
        };
        let value = side.key();

        let mut controls = div("db-view-controls");
        if let Some(pane) = self.pane(side) {
            controls = controls.child(
                div("db-row")
                    .child(chip("x", format!("{} · {}", pane.rendered.x.name, pane.rendered.x.values.len())))
                    .child(chip("y", format!("{} · {}", pane.rendered.y.name, pane.rendered.y.values.len())))
                    .child(chip(
                        "amplitude",
                        format!(
                            "{} … {} {}",
                            fmt_f32(pane.rendered.min),
                            fmt_f32(pane.rendered.max),
                            pane.rendered.units
                        ),
                    ))
                    .child(chip("valid", pane.rendered.valid.to_string())),
            );
        }

        let (frame_label, frame_input) = match self.frame_state_for(side) {
            Some((name, index, len)) => (
                format!("{name} {} / {len}", index + 1),
                Some(self.field_input_sized(frame_field, frame_key, "frame", 5)),
            ),
            None => ("no frame dimension".to_owned(), None),
        };
        let mut frame = div("db-timeline")
            .child(btn("◀", "frame-prev", Some(value), "db-btn"))
            .child(btn(
                if self.playing == Some(side) { "Pause" } else { "Play" },
                "play",
                Some(value),
                "db-btn",
            ))
            .child(btn("▶", "frame-next", Some(value), "db-btn"))
            .child(ui::text::<Msg>(frame_label));
        if let Some(input) = frame_input {
            frame = frame.child(input);
        }
        controls = controls.child(frame);

        let mut readout = div("db-row")
            .child(ui::text::<Msg>("x"))
            .child(self.field_input_sized(ix_field, ix_key, "x", 5))
            .child(ui::text::<Msg>("y"))
            .child(self.field_input_sized(iy_field, iy_key, "y", 5))
            .child(div("db-detail").child(ui::text::<Msg>(self.readout_text(side))));
        if let Some(value) = self.readout_value(side) {
            readout = readout.child(copy_btn("Copy value", &value, "db-btn"));
        }
        if side == Side::Source {
            readout = readout.child(btn(
                if self.linked { "Unlink slice" } else { "Link slice" },
                "link",
                None,
                "db-btn",
            ));
        }
        controls = controls.child(readout);

        let fixed = match side {
            Side::Source => self.project.source_range.is_some(),
            Side::Result => self.project.result_range.is_some(),
        };
        let range = div("db-row")
            .child(chip("range", if fixed { "fixed" } else { "auto" }))
            .child(ui::text::<Msg>("min"))
            .child(self.field_input_sized(min_field, min_key, "min", 7))
            .child(ui::text::<Msg>("max"))
            .child(self.field_input_sized(max_field, max_key, "max", 7))
            .child(btn("Apply", "range", Some(value), "db-btn"))
            .child(btn("Auto", "rangeauto", Some(value), "db-btn"));
        controls.child(range).into()
    }

    fn graph_library(&self) -> Node<Msg> {
        let mut panel = div("db-ops").key("operation-library")
            .child(div("db-panel-h").child(ui::text::<Msg>("Add operation")));
        for (kind, name) in [("gaussian", "Gaussian blur"), ("convolve", "Mean convolution"), ("difference", "Difference · A − B"), ("addition", "Addition · A + B"), ("scale", "Scale & offset"), ("filter", "Filter · Range"), ("median", "Median filter"), ("fft2", "2D FFT · Spectrum"), ("ifft2", "2D IFFT · Inverse"), ("gradient", "Gradient"), ("vector", "Vector · U + V"), ("magnitude", "Magnitude"), ("time_shift", "t+n · Time shift")] {
            panel = panel.child(btn(name, "graph-add", Some(kind), "db-btn db-op"));
        }
        panel.child(div("db-panel-h").child(ui::text::<Msg>("Export")))
            .child(self.field_input(&self.dest_field, "dest", "Output path", true))
            .child(div("db-btns").child(btn("Python", "export", Some("py"), "db-btn primary"))
                .child(btn("PNG", "export", Some("png"), "db-btn"))
                .child(btn("NetCDF", "export", Some("nc"), "db-btn")))
            .into()
    }

    fn graph_canvas(&self) -> Node<Msg> {
        let mut canvas = div("ng-canvas").key("processing-graph");
        if let Some(graph) = &self.project.graph {
            for node in &graph.nodes {
                let id = node.id.to_string();
                let selected = self.selected_node == Some(node.id);
                let mut card = div(if selected { "ng-node on" } else { "ng-node" }).key(format!("node-{}",node.id))
                    .child(btn(&format!("{} · {}", node.id, node.name), "graph-select", Some(&id), "ng-title"));
                for (slot, source) in node.inputs.iter().enumerate() {
                    let label = format!("{}  {}", if slot == 0 { "A" } else { "B" }, source.map_or("Connect input".into(), |s| format!("← node {s}")));
                    card = card.child(btn(&label,"graph-input",Some(&format!("{}:{slot}",node.id)),"ng-port input"));
                }
                let mut parameter = |key: &str, label: String, step: f64| {
                    card = card.clone().child(div("db-btns").child(ui::text::<Msg>(label))
                        .child(btn("−","graph-param",Some(&format!("{}:{key}:{}",node.id,-step)),"db-mini"))
                        .child(btn("+","graph-param",Some(&format!("{}:{key}:{step}",node.id)),"db-mini")));
                };
                match &node.operation {
                    Operation::Gaussian { sigma } => parameter("sigma",format!("σ {sigma:.1} cells"),0.1),
                    Operation::Scale { factor,offset } => { parameter("factor",format!("Factor {factor:.2}"),0.1); parameter("offset",format!("Offset {offset:.2}"),0.1); },
                    Operation::Convolve { side,.. } => { card = card.child(hint(format!("{side} × {side} mean kernel"))); },
                    Operation::TimeShift { steps } => { parameter("steps",format!("Shift {steps:+} frames"),1.0);
                        card = card.child(hint("Positive n shows a later frame, negative n an earlier one. Nested shifts add their offsets. Frames outside the series show as missing.")); },
                    Operation::Filter { min, max } => {
                        if let Some(m) = min { parameter("min", format!("Min {m:.1}"), 0.5); }
                        if let Some(m) = max { parameter("max", format!("Max {m:.1}"), 0.5); }
                        card = card.child(hint("Masks values outside [min, max] as missing (NaN)."));
                    },
                    Operation::Median { radius } => {
                        parameter("radius", format!("Radius {radius} cells"), 1.0);
                    },
                    Operation::Fft2 => { card = card.child(hint("2D FFT magnitude spectrum (log-scale with center shift).")); },
                    Operation::Ifft2 => { card = card.child(hint("2D Inverse FFT reconstructed field.")); },
                    _ => {},
                }
                card = card.child(btn(if self.connect_from == Some(node.id) { "Output selected →" } else { "Output →" },"graph-output",Some(&id),"ng-port output"))
                    .child(div("db-btns").child(btn("Show layer","layer-add",Some(&id),"db-mini"))
                        .child(btn("Remove","graph-remove",Some(&id),"db-mini")));
                canvas = canvas.child(card);
            }
        }
        div("db-pipe").key("node-workspace")
            .child(div("db-pipe-h").child(ui::text::<Msg>("Processing graph"))
                .child(hint("Output → input connects nodes. Changes preview automatically.")))
            .child(canvas).into()
    }

    fn layer_stack(&self) -> Node<Msg> {
        let mut panel = div("db-side ng-layers").key("layer-stack")
            .child(div("db-side-h").child(ui::text::<Msg>("Layers")))
            .child(hint("Top layer is shown first. Colors mix in the composite."));
        if let Some(graph) = &self.project.graph {
            for layer in graph.layers.iter().rev() {
                let id = layer.node.to_string();
                let title = graph.nodes.iter().find(|n| n.id == layer.node).map(|n| n.name.as_str()).unwrap_or("Missing node");
                let mut row = div("ng-layer").key(format!("layer-{}",layer.node))
                    .child(
                        div("ng-layer-head")
                            .child(html::span::<Msg>(title).class("ng-layer-title"))
                            .child(div("db-btns")
                                .child(btn(if layer.visible { "Hide" } else { "Show" },"layer-toggle",Some(&id),"db-mini"))
                                .child(btn("Remove","layer-remove",Some(&id),"db-mini"))),
                    );
                let mut maps = div("ng-maps");
                for map in ["viridis","thermal","diverging","gray"] {
                    maps = maps.child(btn(map,"layer-map",Some(&format!("{}:{map}",layer.node)),if colormap_name(layer.colormap) == map { "db-mini on" } else { "db-mini" }));
                }
                let mut opacity = div("ng-opacity");
                for percent in [0,25,50,75,100] {
                    opacity = opacity.child(btn(&format!("{percent}%"),"layer-opacity",Some(&format!("{}:{}",layer.node,percent as f32/100.0)),if (layer.opacity*100.0-percent as f32).abs()<0.1 { "db-mini on" } else { "db-mini" }));
                }
                row = row.child(maps).child(hint(format!("Opacity {:.0}%",layer.opacity*100.0))).child(opacity)
                    .child(div("db-btns")
                        .child(btn("−5%","layer-opacity",Some(&format!("{}:{}",layer.node,(layer.opacity-0.05).max(0.0))),"db-mini"))
                        .child(btn("+5%","layer-opacity",Some(&format!("{}:{}",layer.node,(layer.opacity+0.05).min(1.0))),"db-mini"))
                        .child(btn("↑","layer-order",Some(&format!("{}:1",layer.node)),"db-mini"))
                        .child(btn("↓","layer-order",Some(&format!("{}:-1",layer.node)),"db-mini")));
                panel = panel.child(row);
            }
        }
        panel.into()
    }


    fn field_input(&self, field: &Field, key: &str, placeholder: &str, grow: bool) -> Node<Msg> {
        let mut node = self.input_node(field, key, placeholder);
        if grow {
            node = node.grow(1.0);
        }
        node
    }

    fn field_input_sized(&self, field: &Field, key: &str, placeholder: &str, width: usize) -> Node<Msg> {
        self.input_node(field, key, placeholder)
            .min(Bound::w(Extent::Ch(width as f64)))
            .max(Bound::w(Extent::Ch(width as f64)))
    }

    fn input_node(&self, field: &Field, key: &str, placeholder: &str) -> Node<Msg> {
        let edit_key = key.to_owned();
        let undo_key = key.to_owned();
        let send_key = key.to_owned();
        let focus_key = key.to_owned();
        ui::input::<Msg>()
            .key(key)
            .text(field.text.clone())
            .cursor(field.cursor)
            .anchor(field.anchor)
            .placeholder(placeholder)
            .sendable(true)
            .on_edit(move |edit| Msg::Edit {
                key: edit_key.clone(),
                from: edit.from,
                to: edit.to,
                text: edit.text.clone(),
                cursor: edit.cursor,
                len: edit.len,
            })
            .on_undo(Msg::Undo { key: undo_key })
            .on_send(move |text| Msg::Send { key: send_key.clone(), text: text.to_owned() })
            .on_focus(Msg::Focus { key: focus_key })
            .into()
    }

    fn field_mut(&mut self, key: &str) -> Option<&mut Field> {
        match key {
            "open" => Some(&mut self.open_field),
            "project" => Some(&mut self.project_field),
            "dest" => Some(&mut self.dest_field),
            "src-ix" => Some(&mut self.src_ix_field),
            "src-iy" => Some(&mut self.src_iy_field),
            "src-frame" => Some(&mut self.src_frame_field),
            "src-min" => Some(&mut self.src_min_field),
            "src-max" => Some(&mut self.src_max_field),
            "dst-ix" => Some(&mut self.dst_ix_field),
            "dst-iy" => Some(&mut self.dst_iy_field),
            "dst-frame" => Some(&mut self.dst_frame_field),
            "dst-min" => Some(&mut self.dst_min_field),
            "dst-max" => Some(&mut self.dst_max_field),
            "script" => Some(&mut self.script),
            _ => None,
        }
    }
}

// ----------------------------------------------------------------- helpers

fn side_of(text: &str) -> Option<Side> {
    match text {
        "source" => Some(Side::Source),
        "result" => Some(Side::Result),
        _ => None,
    }
}

fn div(class: &str) -> html::El<Msg> {
    html::div::<Msg>().class(class)
}

fn chip(key: &str, value: impl Into<String>) -> html::El<Msg> {
    div("db-chip")
        .child(html::span::<Msg>(key).class("db-k"))
        .child(html::span::<Msg>(value.into()).class("db-v"))
}

fn hint(text: impl Into<String>) -> Node<Msg> {
    div("db-hint").child(ui::text::<Msg>(text.into())).into()
}

fn btn(label: &str, act: &str, value: Option<&str>, class: &str) -> Node<Msg> {
    let click = action_string(act, value);
    let name = act.to_owned();
    html::button::<Msg>(label)
        .class(class)
        .actions(Actions::new().click(click))
        .on_action_with(name, |event| Msg::Act(event.act.clone(), event.value.clone()))
        .into()
}

fn copy_btn(label: &str, text: &str, class: &str) -> Node<Msg> {
    html::button::<Msg>(label)
        .class(class)
        .href(text)
        .actions(Actions::new().click("copy"))
        .into()
}

fn clickable(node: html::El<Msg>, act: &str, value: Option<&str>) -> Node<Msg> {
    let click = action_string(act, value);
    let name = act.to_owned();
    node.actions(Actions::new().click(click))
        .on_action_with(name, |event| Msg::Act(event.act.clone(), event.value.clone()))
        .into()
}

fn action_string(act: &str, value: Option<&str>) -> String {
    match value {
        Some(value) => format!("{act}={value}"),
        None => act.to_owned(),
    }
}

fn blob_for(
    image: &Arc<Rendered>,
    old: Option<&PaneView>,
    session: &mut Session<Msg>,
) -> std::result::Result<Option<String>, Error> {
    if let Some(old) = old {
        if Arc::ptr_eq(&old.rendered, image) && old.blob.is_some() {
            return Ok(old.blob.clone());
        }
    }
    session.blob(image.png.clone(), Some("image/png")).map(Some)
}

fn find_field_id(view: &View<Msg>, key: &str) -> Option<String> {
    fn walk(node: &Node<Msg>, id: &str, key: &str, found: &mut Option<String>) {
        if found.is_some() {
            return;
        }
        let explicit = node.props().get("key").and_then(|value| match value {
            Value::String(text) => Some(text.clone()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        });
        if (node.kind() == "input" || node.kind() == "editor") && explicit.as_deref() == Some(key) {
            *found = Some(id.to_owned());
            return;
        }
        for (index, child) in node.child_nodes().iter().enumerate() {
            let child_key = child
                .props()
                .get("key")
                .and_then(|value| match value {
                    Value::String(text) => Some(text.clone()),
                    Value::Number(number) => Some(number.to_string()),
                    _ => None,
                })
                .unwrap_or_else(|| index.to_string());
            walk(child, &format!("{id}.{child_key}"), key, found);
            if found.is_some() {
                return;
            }
        }
    }

    let mut found = None;
    for (slot, name) in view.regions().iter().zip(["main", "dock", "layer"]) {
        if let Some(root) = *slot {
            walk(root, name, key, &mut found);
            if found.is_some() {
                break;
            }
        }
    }
    found
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn byte_at_utf16(text: &str, offset: usize) -> usize {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units >= offset {
            return byte;
        }
        let width = ch.len_utf16();
        if units + width > offset {
            return byte;
        }
        units += width;
    }
    text.len()
}

fn prev_char_byte(text: &str, offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        let width = ch.len_utf16();
        if units + width >= offset {
            return byte;
        }
        units += width;
    }
    text.len()
}

fn next_char_byte(text: &str, offset: usize) -> usize {
    let byte = byte_at_utf16(text, offset);
    match text[byte..].chars().next() {
        Some(ch) => byte + ch.len_utf8(),
        None => text.len(),
    }
}

fn word_start(text: &str, byte: usize) -> usize {
    let mut start = byte;
    while start > 0 {
        let prev = prev_char_byte(text, utf16_len(&text[..start]));
        let ch = text[prev..].chars().next().unwrap_or(' ');
        if !ch.is_whitespace() {
            break;
        }
        start = prev;
    }
    while start > 0 {
        let prev = prev_char_byte(text, utf16_len(&text[..start]));
        let ch = text[prev..].chars().next().unwrap_or(' ');
        if ch.is_whitespace() {
            break;
        }
        start = prev;
    }
    start
}

fn word_end(text: &str, byte: usize) -> usize {
    let mut end = byte;
    while end < text.len() {
        let ch = text[end..].chars().next().unwrap_or(' ');
        if ch.is_whitespace() {
            break;
        }
        end += ch.len_utf8();
    }
    while end < text.len() {
        let ch = text[end..].chars().next().unwrap_or(' ');
        if !ch.is_whitespace() {
            break;
        }
        end += ch.len_utf8();
    }
    end
}

fn line_bounds(text: &str, byte: usize) -> (usize, usize) {
    let start = text[..byte].rfind('\n').map_or(0, |index| index + 1);
    let end = text[byte..].find('\n').map_or(text.len(), |index| byte + index);
    (start, end)
}

fn scan_field_var(text: &str, name: &str) -> Option<String> {
    let prefix = format!("let {name} =");
    for line in text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with(&prefix) {
            continue;
        }
        let Some(at) = trimmed.find("field(") else { continue };
        let rest = &trimmed[at + 6..];
        let Some(start) = rest.find('"') else { continue };
        let after = &rest[start + 1..];
        let Some(end) = after.find('"') else { continue };
        return Some(after[..end].to_owned());
    }
    None
}

fn color_map(name: &str) -> ColorMap {
    match name {
        "thermal" => ColorMap::Thermal,
        "diverging" => ColorMap::Diverging,
        "gray" => ColorMap::Gray,
        _ => ColorMap::Viridis,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    #[test]
    fn source_rail_renders_wrapping_tags_without_dot_concat() {
        let mut app = App::empty();
        app.info = Some(DatasetInfo {
            path: "test.nc".into(),
            variables: vec![Variable {
                name: "uo".into(),
                dimensions: vec![
                    Dimension { name: "time".into(), len: 365, units: "".into() },
                    Dimension { name: "lat".into(), len: 121, units: "".into() },
                    Dimension { name: "lon".into(), len: 121, units: "".into() },
                ],
                units: "m s-1".into(),
                long_name: "velocity".into(),
            }],
        });
        let rail = app.source_rail();
        let debug = format!("{rail:?}");
        assert!(debug.contains("db-var-tags"));
        assert!(debug.contains("db-var-tag"));
        assert!(debug.contains("time"));
        assert!(debug.contains("365"));
        assert!(debug.contains("m s-1"));
        assert!(!debug.contains("time 365 · lat 121"));
    }
}

fn colormap_name(map: ColorMap) -> &'static str {
    match map {
        ColorMap::Viridis => "viridis",
        ColorMap::Thermal => "thermal",
        ColorMap::Diverging => "diverging",
        ColorMap::Gray => "gray",
    }
}

fn range_text(range: Option<[f32; 2]>) -> (String, String) {
    match range {
        Some([low, high]) => (fmt_f32(low), fmt_f32(high)),
        None => (String::new(), String::new()),
    }
}

fn pan_x_class(pan: i32) -> String {
    match pan {
        0 => "db-px0".to_owned(),
        value if value > 0 => format!("db-px{}", value.min(8)),
        value => format!("db-pxm{}", (-value).min(8)),
    }
}

fn pan_y_class(pan: i32) -> String {
    match pan {
        0 => "db-py0".to_owned(),
        value if value > 0 => format!("db-py{}", value.min(8)),
        value => format!("db-pym{}", (-value).min(8)),
    }
}

fn zoom_class(zoom: i32) -> String {
    format!("db-z{}", zoom.clamp(1, 7))
}

fn fmt_f32(value: f32) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value.is_sign_negative() { "-inf" } else { "inf" }.to_owned();
    }
    if value.fract() == 0.0 && value.abs() < 1e9 {
        return format!("{}", value as i64);
    }
    let mut text = format!("{value:.6}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

fn fmt_f64(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    let mut text = format!("{value:.6}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

