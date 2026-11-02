//! The viewer window (port of syncview/app/main.py): toolbar, video, trace rows, overview strip,
//! channel table. Traces are drawn by the GPU line renderer in `gpu.rs`; everything else by egui.

use crate::annotations::{self, Annotations};
use crate::cache::{CacheBuilder, Trace, TraceCache, WindowWorker};
use crate::filters::MODES;
use crate::oe::{check_sync, Recording};
use crate::preset::{self, Clip, Preset, Spec};
use crate::video::{video_duration, VideoDecoder};
use crate::gpu::{px_for, LineStrip};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use eframe::egui_wgpu;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

const RATES: [f64; 10] = [0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 100.0, 300.0];
const TIME_BASES: [f64; 16] = [0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 1800.0, 3600.0, 7200.0];
const WINDOW_MAX_S: f64 = 180.0; // on-demand (uncached) processing only for views up to this span
pub const BG: Color32 = Color32::from_rgb(0x11, 0x11, 0x11);
pub const FG: Color32 = Color32::from_rgb(0xc3, 0xc2, 0xb7);
pub const LABEL: Color32 = Color32::from_rgb(0xe8, 0xe8, 0xe4);
pub const MUTED: Color32 = Color32::from_rgb(0x88, 0x88, 0x88);
const OVERVIEW_SLOT: usize = 10_000;

// layout of the trace area (points), as in the Python viewer
const MARGIN_X: f32 = 9.0;
const LABEL_W: f32 = 130.0;
const YAXIS_W: f32 = 60.0;
const ROW_GAP: f32 = 2.0;
const TAXIS_H: f32 = 20.0;

pub struct Options {
    pub preset: Preset,
    pub videos: Vec<PathBuf>,
    pub trigger_line: i64,
    pub start_time: Option<f64>,
    pub time_base: Option<f64>,
    pub play: bool,
}

const EXTRA_KEYS: [&str; 5] = ["order", "smooth_ms", "env_lp", "plot_fs", "win_s"];

/// Python's "{:g}": up to 6 significant digits, trailing zeros removed.
fn g6(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    let e = x.abs().log10().floor() as i32;
    if !(-5..6).contains(&e) {
        return crate::filters::py_float(x);
    }
    let d = (5 - e).max(0) as usize;
    let s = format!("{x:.d$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

fn fmt_ylim(y: Option<[f64; 2]>) -> String {
    match y {
        None => "auto".into(),
        Some([a, b]) => format!("{}, {}", g6(a), g6(b)),
    }
}

/// Text-field contents of one channel-table row; parsed back into the spec on Enter / focus loss.
struct RowEdit {
    lo: String,
    hi: String,
    notch: String,
    extra: String,
    y: String,
}

impl RowEdit {
    fn from_spec(s: &Spec) -> Self {
        let p = crate::filters::params(s).unwrap_or_default();
        let band = p.get("band").and_then(|b| b.as_array()).cloned().unwrap_or_default();
        let edge = |i: usize| band.get(i).and_then(|v| v.as_f64()).map(g6).unwrap_or_default();
        let notch = match p.get("notch") {
            Some(serde_json::Value::Array(a)) => a.iter().filter_map(|v| v.as_f64()).map(g6).collect::<Vec<_>>().join(","),
            Some(v) => v.as_f64().map(g6).unwrap_or_default(),
            None => String::new(),
        };
        let extra = EXTRA_KEYS
            .iter()
            .filter_map(|k| s.extra.get(*k).and_then(|v| v.as_f64()).map(|v| format!("{k}={}", g6(v))))
            .collect::<Vec<_>>()
            .join(" ");
        RowEdit { lo: edge(0), hi: edge(1), notch, extra, y: fmt_ylim(s.ylim) }
    }

    /// Parse the fields into `s` (fields that don't parse are left as they were).
    fn apply(&self, s: &mut Spec) {
        use serde_json::{json, Value};
        let num = |t: &str| -> Result<Option<f64>, ()> {
            let t = t.trim();
            if t.is_empty() || ["none", "-", "–"].contains(&t.to_lowercase().as_str()) {
                Ok(None)
            } else {
                t.parse::<f64>().map(Some).map_err(|_| ())
            }
        };
        if let (Ok(lo), Ok(hi)) = (num(&self.lo), num(&self.hi)) {
            let current = RowEdit::from_spec(s);
            if self.lo.trim() != current.lo || self.hi.trim() != current.hi {
                s.extra.insert("band".into(), json!([lo, hi]));
            }
        }
        let notch: Result<Vec<f64>, _> =
            self.notch.replace(';', ",").split(',').map(str::trim).filter(|v| !v.is_empty()).map(str::parse::<f64>).collect();
        if let Ok(n) = notch {
            if n.is_empty() {
                s.extra.remove("notch");
            } else {
                s.extra.insert("notch".into(), json!(n));
            }
        }
        let mut seen: Vec<&str> = vec![];
        let text = self.extra.replace(',', " ");
        for kv in text.split_whitespace() {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            if let (Some(k), Ok(v)) = (EXTRA_KEYS.iter().find(|x| **x == k), v.parse::<f64>()) {
                let val = if *k == "order" { json!(v.round() as i64) } else { Value::from(v) };
                s.extra.insert(k.to_string(), val);
                seen.push(k);
            }
        }
        for k in EXTRA_KEYS {
            if !seen.contains(&k) {
                s.extra.remove(k);
            }
        }
        let y: Vec<&str> = self.y.split([',', ';']).map(str::trim).collect();
        if self.y.trim().eq_ignore_ascii_case("auto") || self.y.trim().is_empty() {
            s.ylim = None;
        } else if let [a, b] = y[..] {
            if let (Ok(a), Ok(b)) = (a.parse::<f64>(), b.parse::<f64>()) {
                if a < b {
                    s.ylim = Some([a, b]);
                }
            }
        }
    }
}

struct Row {
    spec: Spec,
    key: String,
    color: Color32,
    auto: Option<(f64, f64)>,
    table_index: usize,
}

pub struct App {
    rec: Arc<Recording>,
    cache: Arc<TraceCache>,
    builder: CacheBuilder,
    window_worker: WindowWorker,
    specs: Vec<Spec>,
    overview: Option<Spec>,
    overview_key: Option<String>,
    rows: Vec<Row>,
    t: f64,
    span: f64,
    rate_i: usize,
    playing: bool,
    last_tick: Option<Instant>,
    /// TTL line for videos added from now on (each video keeps its own).
    trigger_line: i64,
    export: Export,
    /// The videos shown, at most MAX_VIDEOS; the first is the reference for frame stepping, the
    /// frame number and clip export.
    cams: Vec<Cam>,
    edits: Vec<RowEdit>,
    selected: Option<usize>,
    last_dir: Option<PathBuf>,
    span_text: String,
    goto_text: String,
    status: String,
    dialogs: Vec<(String, String)>,
    was_building: bool,
    fps: Option<(Instant, Vec<f64>)>, // SYNCVIEW_RS_FPS=1: per-second frame-time report
    /// Where unclipped traces may draw (video, chart and overview strip), from the last frame.
    free_area: Rect,
    layout: preset::Layout,
    /// The file dialog that is open, if any (see dialog.rs).
    file_dialog: Option<crate::dialog::Pending<FileDialog>>,
    /// The right-hand sidebar: which tab, and whether it is open (F2) or collapsed to a rail.
    side_tab: SideTab,
    side_open: bool,
    /// Annotation labels (from the preset) and this recording's annotated events.
    labels: Vec<preset::Label>,
    ann: Annotations,
    /// Intervals started but not yet ended (label, where it started): one per label at most, so
    /// bites can be marked inside an open chewing bout.
    open_marks: Vec<(String, Mark)>,
    /// Undo / redo of event additions and removals.
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    /// The event just added or restored, and when (drawn brighter for a moment).
    flash: Option<(String, Instant)>,
    annotator: String,
    /// The event selected in the Events tab (edited there; Delete removes it).
    selected_event: Option<String>,
    /// Events tab: show (and step through) only this label's events.
    event_filter: Option<String>,
    /// The selected event's note, as being typed (applied when the field loses focus).
    note_edit: String,
    /// Scroll the events list to the selected event (after stepping to it with the keys).
    list_scroll: bool,
    /// Audit mode: score from the video alone (traces and earlier marks hidden; events marked now
    /// record view "video").
    audit: bool,
    /// The last save error shown, so a failing disk doesn't raise the same dialog every second.
    save_error: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum SideTab {
    Channels,
    Events,
}

/// Where a mark lands: time (s), sample index, and the reference video's frame.
#[derive(Clone, Copy)]
struct Mark {
    t: f64,
    sample: i64,
    frame: Option<i64>,
}

/// Move an event's start (or end) to `m`, keeping start <= end.
fn set_edge(e: &mut annotations::Event, m: Mark, start: bool) {
    if start {
        (e.start_s, e.start_sample, e.frame_start) = (m.t, m.sample, m.frame);
    } else {
        (e.end_s, e.end_sample, e.frame_end) = (m.t, m.sample, m.frame);
    }
    if e.end_s < e.start_s {
        std::mem::swap(&mut e.start_s, &mut e.end_s);
        std::mem::swap(&mut e.start_sample, &mut e.end_sample);
        std::mem::swap(&mut e.frame_start, &mut e.frame_end);
    }
}

/// An undoable change to the annotations.
enum Edit {
    Added(annotations::Event),
    Removed(annotations::Event),
    /// Before, after (boxed: two events are much bigger than the other variants).
    Changed(Box<(annotations::Event, annotations::Event)>),
}

/// At most this many videos at once (e.g. three side views and one from above).
const MAX_VIDEOS: usize = 4;

/// One video, locked to its own TTL line: frame k is shown from the k-th trigger on.
struct Cam {
    dec: VideoDecoder,
    line: i64,
    /// Frame count, width, height and decoder, once the video has opened.
    opened: Option<(usize, usize, usize, String)>,
    /// Once synced: each frame's trigger time (s) and recording row, and the frame period.
    frames: Vec<f64>,
    rows: Vec<i64>,
    period: f64,
    ready: bool,
    tex: Option<egui::TextureHandle>,
    size: [usize; 2],
    caption: String,
    /// Shown instead of the frame while not empty (opening…, errors, outside the video).
    msg: String,
    want: Option<usize>,
}

impl Cam {
    fn new(dec: VideoDecoder, line: i64, msg: String) -> Cam {
        let (frames, rows, tex, caption, want) = (vec![], vec![], None, String::new(), None);
        Cam { dec, line, opened: None, frames, rows, period: 0.02, ready: false, tex, size: [0, 0], caption, msg, want }
    }

    fn name(&self) -> String {
        self.dec.path.file_name().unwrap_or_default().to_string_lossy().into_owned()
    }

    /// Last frame triggered at or before `t` (None outside the video).
    fn frame_at(&self, t: f64) -> Option<usize> {
        let last = *self.frames.last()?;
        if t > last + self.period {
            return None;
        }
        let k = self.frames.partition_point(|x| *x < t + 1e-6) as i64 - 1;
        (k >= 0).then_some(k as usize)
    }

    /// Height / width of its frames (4:3 until the first frame arrives).
    fn aspect(&self) -> f32 {
        let [w, h] = self.size;
        if w > 0 && h > 0 { h as f32 / w as f32 } else { 0.75 }
    }
}

/// Boxes of the given aspects (height / width) side by side in `a`, each as large as its equal
/// share of the width allows, centred in it.
fn tile(a: Rect, aspects: &[f32]) -> Vec<Rect> {
    let w = a.width() / aspects.len().max(1) as f32;
    aspects
        .iter()
        .enumerate()
        .map(|(i, k)| {
            let slot = Rect::from_min_size(Pos2::new(a.left() + i as f32 * w, a.top()), Vec2::new(w, a.height()));
            let bw = slot.width().min(slot.height() / k);
            Rect::from_center_size(slot.center(), Vec2::new(bw, bw * k))
        })
        .collect()
}

/// What an open file dialog's answer is for.
enum FileDialog {
    LoadPreset,
    SavePreset,
    Video,
    SaveClip(ClipPlan),
}

/// A clip export whose settings have been checked, waiting for where to save it.
struct ClipPlan {
    video: PathBuf,
    from: f64,
    to: f64,
    time_base: f64,
    width: Option<u32>,
    speed: f64,
}

/// `b` moved (and shrunk if need be) to lie inside `a`; `aspect` (height / width) is kept.
fn fit_inside(b: Rect, a: Rect, aspect: Option<f32>) -> Rect {
    let size = match aspect {
        Some(k) => {
            let w = b.width().min(a.width()).min(a.height() / k);
            Vec2::new(w, w * k)
        }
        None => b.size().min(a.size()),
    };
    let min = Pos2::new(b.min.x.clamp(a.min.x, a.max.x - size.x), b.min.y.clamp(a.min.y, a.max.y - size.y));
    Rect::from_min_size(min, size)
}

/// Arrange mode: drag box `b` to move it, its bottom-right corner to resize it (keeping `aspect`,
/// at least `min` in size); it stays inside `a`. Returns the new box.
fn drag_box(ui: &egui::Ui, a: Rect, id: &str, b: Rect, aspect: Option<f32>, min: Vec2) -> Rect {
    let handle = Rect::from_center_size(b.right_bottom() - Vec2::splat(6.0), Vec2::splat(16.0));
    let mv = ui.interact(b, ui.id().with((id, "move")), Sense::drag()).on_hover_cursor(egui::CursorIcon::Grab);
    let rs = ui.interact(handle, ui.id().with((id, "resize")), Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeNwSe);
    let new = if rs.dragged() {
        let mut size = b.size() + rs.drag_delta();
        if let Some(k) = aspect {
            size.y = size.x * k;
        }
        Rect::from_min_size(b.min, size.max(min))
    } else if mv.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        b.translate(mv.drag_delta())
    } else {
        b
    };
    fit_inside(new, a, aspect)
}

impl Drop for App {
    /// Unsaved annotations are written when the viewer closes.
    fn drop(&mut self) {
        self.autosave(std::time::Duration::ZERO);
    }
}

/// The Clip column: a three-position slider switch (row | chart | none); click or drag a stop.
/// True if it changed.
fn clip_switch(ui: &mut egui::Ui, clip: &mut Clip) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(46.0, 20.0), Sense::click_and_drag());
    let stop_x = |k: usize| rect.left() + 7.0 + k as f32 * (rect.width() - 14.0) / 2.0;
    let mut changed = false;
    if (resp.clicked() || resp.dragged())
        && let Some(p) = resp.interact_pointer_pos()
    {
        let k = ((p.x - stop_x(0)) / (stop_x(1) - stop_x(0))).round().clamp(0.0, 2.0) as usize;
        if *clip != Clip::ALL[k] {
            *clip = Clip::ALL[k];
            changed = true;
        }
    }
    let k = Clip::ALL.iter().position(|c| c == clip).unwrap_or(0);
    let v = ui.visuals();
    let y = rect.center().y;
    let track = Rect::from_min_max(Pos2::new(stop_x(0) - 4.0, y - 3.0), Pos2::new(stop_x(2) + 4.0, y + 3.0));
    let p = ui.painter();
    p.rect_filled(track, 3.0, v.widgets.inactive.bg_fill);
    for j in 0..3 {
        p.circle_filled(Pos2::new(stop_x(j), y), 1.5, MUTED);
    }
    // the knob stands out once the trace is allowed past its row
    let knob = if *clip == Clip::Row { FG } else { v.selection.bg_fill };
    let r = if resp.hovered() { 6.5 } else { 6.0 };
    p.circle(Pos2::new(stop_x(k), y), r, knob, Stroke::new(1.0, BG));
    resp.on_hover_text(format!("clip at: {}   (row | chart | none)", clip.name()));
    changed
}

fn hex(s: &str) -> Color32 {
    Color32::from_hex(s).unwrap_or(FG)
}

pub fn fmt_time(t: f64, span: Option<f64>) -> String {
    let sign = if t < 0.0 { "-" } else { "" };
    let dec = match span {
        None => 3,
        Some(sp) if sp < 10.0 => 3,
        Some(sp) if sp < 300.0 => 1,
        _ => 0,
    };
    // round first, so 59.98 s at 0 decimals carries into the minutes ("7:00", not "6:60")
    let scale = 10f64.powi(dec);
    let t = (t.abs() * scale).round() / scale;
    let h = (t / 3600.0).floor();
    let rem = t - h * 3600.0;
    let m = (rem / 60.0).floor();
    let s = rem - m * 60.0;
    let dec = dec as usize;
    let width = if dec > 0 { 3 + dec } else { 2 };
    let sec = format!("{s:0width$.dec$}");
    if h > 0.0 { format!("{sign}{}:{:02}:{sec}", h as i64, m as i64) } else { format!("{sign}{}:{sec}", m as i64) }
}

/// Python's "%.3g" for the magnitudes used here.
fn g3(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    let d = (2 - x.abs().log10().floor() as i32).max(0) as usize;
    let s = format!("{x:.d$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

fn fmt_span(s: f64) -> String {
    if s < 60.0 {
        format!("{} s", g3(s))
    } else if s < 3600.0 {
        format!("{} min", g3(s / 60.0))
    } else {
        format!("{} h", g3(s / 3600.0))
    }
}

fn parse_span(text: &str) -> Option<f64> {
    let t: String = text.trim().to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    for (suf, mul) in [("min", 60.0), ("h", 3600.0), ("ms", 1e-3), ("s", 1.0), ("m", 60.0)] {
        if let Some(v) = t.strip_suffix(suf) {
            return v.parse::<f64>().ok().map(|v| v * mul);
        }
    }
    t.parse().ok()
}

const NICE_STEPS: [f64; 23] = [1e-3, 2e-3, 5e-3, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0,
    300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0];

fn nice_spacing(span: f64, n_major: f64) -> (f64, f64) {
    let major = NICE_STEPS.iter().copied().find(|s| span / s <= n_major).unwrap_or(7200.0);
    let minor = NICE_STEPS
        .iter()
        .rev()
        .copied()
        .find(|s| *s < major && [2.0, 2.5, 3.0, 4.0, 5.0, 6.0].iter().any(|r| ((major / s) - r).abs() < 1e-9))
        .unwrap_or(major / 5.0);
    (major, minor)
}

fn fmt_tick(v: f64, step: f64) -> String {
    if v.abs() < step * 1e-6 {
        return "0".into();
    }
    let mut d = 0;
    while d < 6 && ((step * 10f64.powi(d)) - (step * 10f64.powi(d)).round()).abs() > 1e-6 {
        d += 1;
    }
    format!("{v:.prec$}", prec = d as usize)
}

impl App {
    /// The viewer for `rec`. The GPU line renderer must already be set up (`gpu::init`).
    pub fn new(ctx: &egui::Context, rec: Arc<Recording>, cache: Arc<TraceCache>, opts: Options) -> Self {
        let ctx = ctx.clone();
        let c1 = ctx.clone();
        let builder = CacheBuilder::new(cache.clone(), move || c1.request_repaint());
        let c2 = ctx.clone();
        let window_worker = WindowWorker::new(cache.clone(), move || c2.request_repaint());
        let frames: Vec<f64> = rec.rising_edges(opts.trigger_line).iter().map(|i| *i as f64 / rec.fs).collect();
        let t = opts.start_time.unwrap_or(frames.first().copied().unwrap_or(0.0)).clamp(0.0, rec.duration());
        let mut app = App {
            builder,
            window_worker,
            specs: vec![],
            overview: None,
            overview_key: None,
            rows: vec![],
            t,
            span: 10.0,
            rate_i: 3,
            playing: false,
            last_tick: None,
            trigger_line: opts.trigger_line,
            export: Export::default(),
            cams: vec![],
            edits: vec![],
            selected: None,
            last_dir: None,
            span_text: String::new(),
            goto_text: String::new(),
            status: String::new(),
            dialogs: vec![],
            was_building: false,
            fps: std::env::var_os("SYNCVIEW_RS_FPS").map(|_| (Instant::now(), vec![])),
            free_area: Rect::NOTHING,
            layout: preset::Layout::default(),
            file_dialog: None,
            side_tab: SideTab::Channels,
            side_open: true,
            labels: preset::default_labels(),
            open_marks: vec![],
            undo: vec![],
            redo: vec![],
            flash: None,
            selected_event: None,
            event_filter: None,
            note_edit: String::new(),
            list_scroll: false,
            audit: false,
            annotator: std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default(),
            ann: Annotations::load(
                annotations::file_for(&rec.rec_dir),
                annotations::Header {
                    recording: rec.rec_dir.display().to_string(),
                    sample_rate: rec.fs,
                    first_sample_number: rec.first_sample_number(),
                    labels: preset::default_labels().iter().map(preset::Label::describe).collect(),
                },
            ),
            save_error: None,
            rec,
            cache,
        };
        app.apply_preset(opts.preset);
        if let Some(tb) = opts.time_base {
            app.set_span(tb);
        }
        for v in opts.videos {
            app.attach_video(v, &ctx);
        }
        if opts.play {
            app.toggle_play();
        }
        app
    }

    fn apply_preset(&mut self, p: Preset) {
        let (mut p, mut problems) = preset::check(p, &self.rec);
        if !problems.is_empty() {
            for m in &problems {
                eprintln!("syncview: preset: {m}");
            }
            if p.channels.is_empty() {
                problems.push("No usable rows left - showing the recording's own channels instead.".into());
                p = preset::from_recording(&self.rec);
            }
            let body = problems.iter().map(|m| format!("• {m}")).collect::<Vec<_>>().join("\n");
            self.dialogs.push(("Channel preset doesn't match this recording".into(), body));
        }
        self.set_span(p.time_base.unwrap_or(self.span));
        if let Some(l) = p.layout {
            self.layout = l;
        }
        if !p.labels.is_empty() {
            self.labels = p.labels;
            self.ann.header.labels = self.labels.iter().map(preset::Label::describe).collect();
        }
        self.overview = p.overview;
        self.specs = p.channels;
        self.selected = None;
        self.sync_edits();
        self.set_specs();
    }

    fn set_specs(&mut self) {
        if self.overview.is_none() {
            // default overview: slow-wave power of the first slow row
            if let Some(s) = self.specs.iter().find(|s| s.mode == "slow") {
                let mut o = Spec::new(&s.ch, "bandpower");
                o.reference = s.reference.clone();
                o.label = Some(format!("{} slow-wave power", s.title()));
                self.overview = Some(o);
            }
        }
        self.overview_key = self.overview.as_ref().map(|o| self.cache.key(o));
        let old: std::collections::HashMap<String, Option<(f64, f64)>> = self.rows.iter().map(|r| (r.key.clone(), r.auto)).collect();
        let shown: Vec<&Spec> = self.specs.iter().filter(|s| s.show).collect();
        let colors = crate::clip::row_colors(&shown);
        self.rows = self
            .specs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.show)
            .enumerate()
            .map(|(i, (ti, s))| {
                let key = self.cache.key(s);
                Row {
                    color: hex(&colors[i]),
                    auto: old.get(&key).copied().flatten(),
                    key,
                    spec: s.clone(),
                    table_index: ti,
                }
            })
            .collect();
        // only build what is needed; the overview trace goes first
        let mut want: Vec<Spec> = self.overview.iter().cloned().collect();
        want.extend(self.rows.iter().map(|r| r.spec.clone()));
        self.cache.retain(&want);
        self.builder.request(want, &self.cache);
        let keys: std::collections::HashSet<&String> = self.rows.iter().map(|r| &r.key).collect();
        self.window_worker.results.lock().unwrap().retain(|k, _| keys.contains(k));
    }

    // ------------------------------------------------------------------ clock
    fn set_time(&mut self, t: f64) {
        self.t = t.clamp(0.0, self.rec.duration());
    }

    fn set_span(&mut self, s: f64) {
        self.span = s.clamp(0.05, self.rec.duration().max(0.05));
        self.span_text = fmt_span((self.span * 1000.0).round() / 1000.0);
    }

    /// The reference video (the first), once synced.
    fn cam0(&self) -> Option<&Cam> {
        self.cams.first().filter(|c| c.ready)
    }

    /// Step n frames of the reference video (n × 20 ms without one).
    fn step_frames(&mut self, n: i64) {
        let Some(c) = self.cam0() else {
            self.set_time(self.t + n as f64 * 0.02);
            return;
        };
        let k = c.frames.partition_point(|x| *x < self.t + 1e-6) as i64 - 1;
        let k = (k + n).clamp(0, c.frames.len() as i64 - 1) as usize;
        let t = c.frames[k];
        self.set_time(t);
    }

    /// The reference video's frame at the cursor (None outside it).
    fn current_frame(&self) -> Option<usize> {
        self.cam0()?.frame_at(self.t)
    }

    fn toggle_play(&mut self) {
        self.playing = !self.playing;
        self.last_tick = self.playing.then(Instant::now);
    }

    fn tick(&mut self) {
        if !self.playing {
            return;
        }
        let now = Instant::now();
        let dt = self.last_tick.map_or(0.0, |l| (now - l).as_secs_f64());
        self.last_tick = Some(now);
        if self.t >= self.rec.duration() {
            self.toggle_play();
            return;
        }
        self.set_time(self.t + dt * RATES[self.rate_i]);
    }

    // ------------------------------------------------------------------ video
    /// Show another video (up to MAX_VIDEOS), locked to the current TTL line.
    pub fn attach_video(&mut self, path: PathBuf, ctx: &egui::Context) {
        if self.cams.iter().any(|c| c.dec.path == path) {
            self.status = format!("{} is already shown", path.display());
            return;
        }
        if self.cams.len() >= MAX_VIDEOS {
            self.dialogs.push(("Too many videos".into(), format!("syncview shows up to {MAX_VIDEOS} videos; remove one (Videos menu in the toolbar) first.")));
            return;
        }
        let c = ctx.clone();
        let msg = format!("opening {} …", path.file_name().unwrap_or_default().to_string_lossy());
        let dec = VideoDecoder::new(path.clone(), self.cache.root.join("video_index"), move || c.request_repaint());
        self.cams.push(Cam::new(dec, self.trigger_line, msg));
    }

    fn remove_video(&mut self, i: usize) {
        self.cams.remove(i);
        if i < self.layout.videos.len() {
            self.layout.videos.remove(i);
        }
    }

    /// Lock video i's frames to the triggers on its TTL line (again, after the line changes).
    fn sync_cam(&mut self, i: usize) {
        let c = &mut self.cams[i];
        let Some((n, w, h, backend)) = c.opened.clone() else { return };
        let name = c.name();
        c.ready = false;
        c.want = None;
        match check_sync(&self.rec, n, c.line, video_duration(&c.dec.path)) {
            Err(e) => {
                eprintln!("syncview: sync error ({name}): {e}");
                c.msg = format!("{name}: {e}");
                c.frames.clear();
                c.rows.clear();
                self.dialogs.push(("Video sync failed".into(), format!("{name} on TTL line {}:\n\n• {e}", c.line)));
            }
            Ok(si) => {
                for m in &si.issues {
                    eprintln!("syncview: sync warning ({name}): {m}");
                }
                if !si.issues.is_empty() {
                    let body = si.issues.iter().map(|m| format!("• {m}")).collect::<Vec<_>>().join("\n\n");
                    self.dialogs.push(("Video sync warning".into(), format!("{name} on TTL line {}:\n\n{body}", c.line)));
                }
                c.frames = si.frames.iter().map(|i| *i as f64 / self.rec.fs).collect();
                c.rows = si.frames;
                c.period = si.period;
                c.ready = true;
                c.msg.clear();
                let (first, last) = (c.frames[0], *c.frames.last().unwrap());
                self.status = format!(
                    "{name}: {n} frames {w}×{h} on TTL line {}, {} extra trigger(s) dropped; video spans {} – {}; decoder: {backend}",
                    c.line, si.extra, fmt_time(first, None), fmt_time(last, None)
                );
                if i == 0 && (self.t < first || self.t > last) {
                    self.t = first;
                }
            }
        }
    }

    fn poll_video(&mut self, ctx: &egui::Context) {
        for i in 0..self.cams.len() {
            match self.cams[i].dec.take_opened() {
                Some(Ok(info)) => {
                    self.cams[i].opened = Some((info.n_frames, info.width, info.height, info.backend));
                    self.sync_cam(i);
                }
                Some(Err(msg)) => {
                    eprintln!("syncview: {msg}");
                    self.cams[i].msg = msg;
                }
                None => {}
            }
            let t = self.t;
            let c = &mut self.cams[i];
            if let Some(f) = c.dec.take_frame() {
                let img = egui::ColorImage::from_rgb([f.width, f.height], &f.rgb);
                match &mut c.tex {
                    Some(tex) => tex.set(img, egui::TextureOptions::LINEAR),
                    None => c.tex = Some(ctx.load_texture(format!("video{i}"), img, egui::TextureOptions::LINEAR)),
                }
                c.size = [f.width, f.height];
                let trig = c.frames.get(f.index).map(|t| fmt_time(*t, None)).unwrap_or_default();
                c.caption = format!("frame {}   trigger {}", f.index, trig);
            }
            if c.ready {
                match c.frame_at(t) {
                    None => {
                        c.want = None;
                        c.msg = "no video at this time".into();
                    }
                    Some(k) => {
                        c.msg.clear();
                        if Some(k) != c.want {
                            c.want = Some(k);
                            c.dec.request(k);
                        }
                    }
                }
            }
        }
    }

    /// Stacked layout: the videos side by side, above the chart.
    fn video_ui(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        ui.allocate_rect(rect, Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0.0, Color32::BLACK);
        let aspects: Vec<f32> = self.cams.iter().map(Cam::aspect).collect();
        for (i, b) in tile(rect, &aspects).into_iter().enumerate() {
            self.paint_cam(&p, i, b);
        }
    }

    /// Video i's current frame (or its message) in `target`, with its caption.
    fn paint_cam(&self, p: &egui::Painter, i: usize, target: Rect) {
        let c = &self.cams[i];
        p.rect_filled(target, 0.0, Color32::BLACK);
        match &c.tex {
            Some(tex) if c.msg.is_empty() => {
                p.image(tex.id(), target, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                p.text(target.left_bottom() + Vec2::new(8.0, -6.0), Align2::LEFT_BOTTOM, &c.caption, FontId::monospace(11.0), LABEL);
            }
            _ => {
                let msg = if c.msg.is_empty() { "…" } else { &c.msg };
                p.text(target.center(), Align2::CENTER_CENTER, msg, FontId::proportional(13.0), MUTED);
            }
        }
    }

    fn has_video(&self) -> bool {
        !self.cams.is_empty()
    }

    /// Video i's floating box, as fractions of the viewing area (see Layout::videos).
    fn video_frac(&self, i: usize) -> [f32; 3] {
        self.layout.videos.get(i).copied().unwrap_or(preset::VIDEO_BOXES[i.min(preset::VIDEO_BOXES.len() - 1)])
    }

    fn set_video_frac(&mut self, i: usize, f: [f32; 3]) {
        while self.layout.videos.len() <= i {
            let k = self.layout.videos.len();
            self.layout.videos.push(self.video_frac(k));
        }
        self.layout.videos[i] = f;
    }

    /// Whether the video or the chart floats (otherwise the video sits above the chart).
    fn floating(&self) -> bool {
        (self.layout.float_video && self.has_video()) || self.layout.float_chart
    }

    /// Where the videos and the chart go in the viewing area `a`: each is its floating box, or
    /// else the videos are fitted side by side into the area and the chart fills it.
    fn boxes(&self, a: Rect) -> (Vec<Rect>, Rect) {
        let l = &self.layout;
        let aspects: Vec<f32> = self.cams.iter().map(Cam::aspect).collect();
        let videos = if l.float_video {
            aspects
                .iter()
                .enumerate()
                .map(|(i, k)| {
                    let [x, y, w] = self.video_frac(i);
                    let w = w * a.width();
                    fit_inside(Rect::from_min_size(a.min + Vec2::new(x * a.width(), y * a.height()), Vec2::new(w, w * k)), a, Some(*k))
                })
                .collect()
        } else {
            tile(a, &aspects)
        };
        let chart = if l.float_chart {
            let [x, y, w, h] = l.chart;
            fit_inside(Rect::from_min_size(a.min + Vec2::new(x * a.width(), y * a.height()), Vec2::new(w * a.width(), h * a.height())), a, None)
        } else {
            a
        };
        (videos, chart)
    }

    /// The viewing area when the video or the chart floats. Unlocked, drags move and resize the
    /// floating boxes (the chart, drawn on top, takes the drag where they overlap); locked, drags
    /// pan time. The video is darkened where the chart covers it.
    fn view_ui(&mut self, ui: &mut egui::Ui) {
        let a = ui.available_rect_before_wrap();
        ui.allocate_rect(a, Sense::hover());
        if a.width() < 60.0 || a.height() < 60.0 {
            return;
        }
        let (videos, chart) = self.boxes(a);
        let resp = ui.interact(chart, ui.id().with("chart"), Sense::click_and_drag());
        if !self.layout.locked {
            let fx = |b: Rect| (b.min.x - a.min.x) / a.width();
            let fy = |b: Rect| (b.min.y - a.min.y) / a.height();
            if self.layout.float_video {
                // later videos are drawn on top, so they take the drag where boxes overlap
                for (i, v) in videos.iter().enumerate() {
                    let k = self.cams[i].aspect();
                    let b = drag_box(ui, a, &format!("video_box{i}"), *v, Some(k), Vec2::new(80.0, 80.0 * k));
                    self.set_video_frac(i, [fx(b), fy(b), b.width() / a.width()]);
                }
            }
            if self.layout.float_chart {
                let b = drag_box(ui, a, "chart_box", chart, None, Vec2::new(240.0, 140.0));
                self.layout.chart = [fx(b), fy(b), b.width() / a.width(), b.height() / a.height()];
            }
        }
        let (videos, chart) = self.boxes(a);
        let p = ui.painter_at(a);
        p.rect_filled(a, 0.0, BG);
        for (i, v) in videos.iter().enumerate() {
            self.paint_cam(&p, i, *v);
            let under = v.intersect(chart);
            if self.layout.dim > 0.0 && under.is_positive() {
                p.rect_filled(under, 0.0, Color32::from_black_alpha((self.layout.dim.clamp(0.0, 1.0) * 255.0) as u8));
            }
        }
        let pointer_in = ui.ui_contains_pointer() && ui.ctx().pointer_hover_pos().is_some_and(|q| chart.contains(q));
        self.traces_ui(ui, chart, &resp, pointer_in);
        if !self.layout.locked {
            // arrange mode: outline each floating box and mark its resize corner
            let mut floating: Vec<Rect> = if self.layout.float_video { videos } else { vec![] };
            if self.layout.float_chart {
                floating.push(chart);
            }
            let p = ui.painter_at(a);
            for b in floating {
                p.rect_stroke(b, 0.0, Stroke::new(1.0, MUTED), egui::StrokeKind::Inside);
                let c = b.right_bottom();
                p.add(egui::Shape::convex_polygon(vec![c, c - Vec2::new(12.0, 0.0), c - Vec2::new(0.0, 12.0)], MUTED, Stroke::NONE));
            }
        }
    }

    // ------------------------------------------------------------------ data
    fn trace_for(&self, row: &Row) -> Option<Arc<Trace>> {
        if let Some(t) = self.cache.get(&row.spec) {
            return Some(t);
        }
        let res = self.window_worker.results.lock().unwrap();
        let (lo, hi, tr) = res.get(&row.key)?;
        (*lo <= self.t - self.span / 2.0 && self.t + self.span / 2.0 <= *hi).then(|| tr.clone())
    }

    fn auto_ylim(mode: &str, tr: &Trace) -> (f64, f64) {
        if tr.stats.is_empty() {
            return (-1.0, 1.0);
        }
        let s = |k: &str| tr.stat(k).unwrap_or(1.0);
        match mode {
            "hilo" => {
                let m = s("absmax99.9") * 2.0;
                (-m, m)
            }
            "envelope" | "bandpower" => (0.0, s("99.9") * 1.2),
            _ => {
                let (lo, hi) = (s("0.5"), s("99.5"));
                let r = hi - lo;
                (lo - 0.1 * r, hi + 0.1 * r)
            }
        }
    }

    fn row_ylim(&mut self, i: usize, tr: Option<&Trace>) -> (f64, f64) {
        let row = &mut self.rows[i];
        if let Some([lo, hi]) = row.spec.ylim {
            return (lo, hi);
        }
        if row.auto.is_none() {
            if let Some(tr) = tr {
                row.auto = Some(Self::auto_ylim(&row.spec.mode, tr));
            }
        }
        row.auto.unwrap_or((-1.0, 1.0))
    }

    fn scale_row_y(&mut self, i: usize, f: f64) {
        let tr = self.trace_for(&self.rows[i]);
        let (lo, hi) = self.row_ylim(i, tr.as_deref());
        let row = &mut self.rows[i];
        let new = if matches!(row.spec.mode.as_str(), "envelope" | "bandpower") {
            [lo, lo + (hi - lo) * f]
        } else {
            let c = (lo + hi) / 2.0;
            [c - (hi - lo) / 2.0 * f, c + (hi - lo) / 2.0 * f]
        };
        row.spec.ylim = Some(new);
        self.specs[row.table_index].ylim = Some(new);
        self.edits[row.table_index].y = fmt_ylim(Some(new));
    }

    fn reset_row_y(&mut self, i: usize) {
        let row = &mut self.rows[i];
        row.spec.ylim = None;
        row.auto = None;
        self.specs[row.table_index].ylim = None;
        self.edits[row.table_index].y = fmt_ylim(None);
    }

    // ------------------------------------------------------------------ input
    fn keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        use egui::Key;
        // annotation: label keys, Esc (drop the last open interval), undo / redo, Ctrl/Cmd+arrows
        // (previous / next event), Delete (the selected event)
        let (mut step, mut delete) = (vec![], false);
        let (labels, esc, undo, redo) = ctx.input_mut(|i| {
            let mut hit = vec![];
            for (n, l) in self.labels.iter().enumerate() {
                if let Some(k) = Key::from_name(&l.key.to_uppercase())
                    && i.consume_key(egui::Modifiers::NONE, k)
                {
                    hit.push(n);
                }
            }
            let (mut undo, mut redo) = (false, false);
            for e in &i.events {
                if let egui::Event::Key { key, pressed: true, modifiers, .. } = e
                    && modifiers.command
                {
                    match key {
                        Key::Z if modifiers.shift => redo = true,
                        Key::Z => undo = true,
                        Key::Y => redo = true,
                        Key::ArrowLeft => step.push(-1),
                        Key::ArrowRight => step.push(1),
                        _ => {}
                    }
                }
            }
            delete = i.consume_key(egui::Modifiers::NONE, Key::Delete) || i.consume_key(egui::Modifiers::NONE, Key::Backspace);
            (hit, i.consume_key(egui::Modifiers::NONE, Key::Escape), undo, redo)
        });
        for d in step {
            self.step_event(d);
        }
        if delete {
            self.delete_selected();
        }
        for n in labels {
            self.label_key(n);
        }
        if esc && let Some((label, m)) = self.open_marks.pop() {
            self.status = format!("{label} from {} cancelled", fmt_time(m.t, None));
        }
        if undo {
            self.undo_edit();
        }
        if redo {
            self.redo_edit();
        }
        // every press in order (several can arrive in one frame: fast typing, key repeat)
        let (pressed, shift) = ctx.input(|i| {
            let keys = [Key::ArrowLeft, Key::ArrowRight, Key::PageUp, Key::PageDown, Key::Home, Key::End, Key::Space,
                        Key::OpenBracket, Key::CloseBracket, Key::Plus, Key::Equals, Key::Minus];
            let pressed = i
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key { key, pressed: true, modifiers, .. } if keys.contains(key) && !modifiers.command => Some(*key),
                    _ => None,
                })
                .collect::<Vec<_>>();
            (pressed, i.modifiers.shift)
        });
        for k in pressed {
            match k {
                Key::ArrowLeft | Key::ArrowRight => {
                    let d = if k == Key::ArrowLeft { -1 } else { 1 };
                    if shift { self.set_time(self.t + d as f64 * 0.1 * self.span) } else { self.step_frames(d) }
                }
                Key::PageUp => self.set_time(self.t - self.span),
                Key::PageDown => self.set_time(self.t + self.span),
                Key::Home => self.set_time(0.0),
                Key::End => self.set_time(self.rec.duration()),
                Key::Space => self.toggle_play(),
                Key::OpenBracket => self.rate_i = self.rate_i.saturating_sub(1),
                Key::CloseBracket => self.rate_i = (self.rate_i + 1).min(RATES.len() - 1),
                Key::Plus | Key::Equals => self.set_span(self.span * 0.8),
                Key::Minus => self.set_span(self.span / 0.8),
                _ => {}
            }
        }
    }

    /// Mouse wheel / trackpad over the trace rows: zoom time, pan, or (⌘/Ctrl) scale one row's Y.
    fn wheel(&mut self, ctx: &egui::Context, row: Option<usize>, plot_w: f32) {
        let events = ctx.input(|i| i.events.clone());
        for e in events {
            let egui::Event::MouseWheel { unit, delta, modifiers, .. } = e else { continue };
            let per_step = match unit {
                egui::MouseWheelUnit::Line => 1.0,
                egui::MouseWheelUnit::Point => 1.0 / 50.0,
                egui::MouseWheelUnit::Page => 10.0,
            };
            let (sx, sy) = (delta.x as f64 * per_step, delta.y as f64 * per_step);
            if modifiers.command {
                if let Some(r) = row {
                    self.scale_row_y(r, 0.8f64.powf(sy));
                }
            } else if sx.abs() > sy.abs() {
                // sideways swipe: move the data with the fingers
                let dt = if unit == egui::MouseWheelUnit::Point { delta.x as f64 * self.span / plot_w as f64 } else { sx * 0.1 * self.span };
                self.set_time(self.t - dt);
            } else if modifiers.shift {
                self.set_time(self.t - sy * 0.1 * self.span);
            } else if sy != 0.0 {
                self.set_span(self.span * 0.8f64.powf(sy));
            }
        }
    }

    // ------------------------------------------------------------------ panels
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.add_sized([36.0, 22.0], egui::Button::new(if self.playing { "⏸" } else { "▶" })).clicked() {
                self.toggle_play();
            }
            egui::ComboBox::from_id_salt("rate")
                .width(60.0)
                .selected_text(format!("{}×", RATES[self.rate_i]))
                .show_ui(ui, |ui| {
                    for (i, r) in RATES.iter().enumerate() {
                        ui.selectable_value(&mut self.rate_i, i, format!("{r}×"));
                    }
                });
            ui.separator();
            ui.label("time base");
            let resp = ui.add(egui::TextEdit::singleline(&mut self.span_text).desired_width(70.0));
            if resp.lost_focus() {
                if let Some(s) = parse_span(&self.span_text) {
                    self.set_span(s);
                } else {
                    self.set_span(self.span);
                }
            }
            egui::ComboBox::from_id_salt("span").width(20.0).selected_text("").show_ui(ui, |ui| {
                for s in TIME_BASES {
                    if ui.selectable_label(false, fmt_span(s)).clicked() {
                        self.set_span(s);
                    }
                }
            });
            ui.separator();
            ui.label("go to");
            let resp = ui.add(egui::TextEdit::singleline(&mut self.goto_text).hint_text("h:mm:ss or seconds").desired_width(130.0));
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let parts: Option<Vec<f64>> = self.goto_text.trim().split(':').map(|p| p.parse().ok()).collect();
                if let Some(parts) = parts {
                    let t = parts.iter().rev().enumerate().map(|(i, v)| v * 60f64.powi(i as i32)).sum();
                    self.set_time(t);
                }
            }
            ui.separator();
            self.videos_menu(ui);
            let can_export = self.cam0().is_some();
            if ui
                .add_enabled(can_export, egui::Button::new("Export clip…"))
                .on_hover_text("save the video with the traces scrolling underneath as an MP4")
                .on_disabled_hover_text("attach a video first")
                .clicked()
            {
                self.open_export();
            }
            ui.separator();
            let has_video = self.has_video();
            if ui
                .add_enabled(has_video, egui::Button::selectable(self.layout.float_video, "Float video"))
                .on_hover_text("make the video a box you can move and resize (unlocked: drag it, or its corner)")
                .on_disabled_hover_text("attach a video first")
                .clicked()
            {
                self.layout.float_video = !self.layout.float_video;
            }
            if ui
                .add(egui::Button::selectable(self.layout.float_chart, "Float chart"))
                .on_hover_text("make the chart a box you can move and resize, over the video (unlocked: drag it, or its corner)")
                .clicked()
            {
                self.layout.float_chart = !self.layout.float_chart;
            }
            if self.floating() {
                let lock = if self.layout.locked { "🔒" } else { "🔓" };
                if ui.add(egui::Button::selectable(self.layout.locked, lock))
                    .on_hover_text(if self.layout.locked {
                        "layout locked: drags pan time (click to unlock and arrange the floating boxes)"
                    } else {
                        "arranging: drags move and resize the floating boxes; click to lock them, so drags pan time"
                    })
                    .clicked()
                {
                    self.layout.locked = !self.layout.locked;
                }
                ui.label("dim").on_hover_text("darken the video where the chart covers it");
                ui.add(egui::Slider::new(&mut self.layout.dim, 0.0..=0.9).show_value(false)).on_hover_text(format!("{:.0} % darker", self.layout.dim * 100.0));
            }
            ui.checkbox(&mut self.layout.outline, "outline").on_hover_text("a dark line under each trace, so it stands out (e.g. over the video)");
            if self.layout.outline {
                ui.add(egui::DragValue::new(&mut self.layout.outline_px).range(0.5..=4.0).speed(0.05).max_decimals(1).suffix(" px"))
                    .on_hover_text("how far the outline extends beyond the trace on each side");
            }
            ui.separator();
            let fr = self.current_frame().map(|k| format!("   frame {k}")).unwrap_or_default();
            ui.label(egui::RichText::new(format!("t = {}  ({:.3} s){fr}", fmt_time(self.t, None), self.t)).monospace().size(13.0));
        });
    }

    /// The videos shown: each one's TTL line, Remove, and Add video… (up to MAX_VIDEOS).
    fn videos_menu(&mut self, ui: &mut egui::Ui) {
        let n = self.cams.len();
        let title = if n == 0 { "Video…".to_string() } else { format!("Videos ({n})") };
        let (mut remove, mut add, mut resync) = (None, false, None);
        if n == 0 {
            add = ui.button(title).on_hover_text("attach a video recorded during this session").clicked();
        } else {
            ui.menu_button(title, |ui| {
                egui::Grid::new("videos").num_columns(4).spacing([10.0, 6.0]).show(ui, |ui| {
                    for (i, c) in self.cams.iter_mut().enumerate() {
                        ui.label(egui::RichText::new(format!("{}  {}", i + 1, c.name())).monospace());
                        ui.label("TTL line");
                        let r = ui.add(egui::DragValue::new(&mut c.line).range(1..=64)).on_hover_text("the line carrying this camera's one pulse per frame");
                        if r.drag_stopped() || (r.changed() && !r.dragged()) {
                            resync = Some(i);
                        }
                        if ui.button("Remove").clicked() {
                            remove = Some(i);
                        }
                        ui.end_row();
                    }
                });
                ui.label(egui::RichText::new("Video 1 sets the frame number, frame stepping and clip export.").small().color(MUTED));
                if ui.add_enabled(n < MAX_VIDEOS, egui::Button::new("Add video…")).on_disabled_hover_text(format!("at most {MAX_VIDEOS} videos")).clicked() {
                    add = true;
                    ui.close();
                }
            });
        }
        if let Some(i) = resync {
            self.sync_cam(i);
        }
        if let Some(i) = remove {
            self.remove_video(i);
        }
        if add {
            let ctx = ui.ctx().clone();
            self.choose_video_dialog(&ctx);
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let building = *self.builder.progress.lock().unwrap();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(f) = building {
                    ui.add(egui::ProgressBar::new(f as f32).desired_width(220.0).text(format!("filtering session… {:.0}%", f * 100.0)));
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(&self.status).size(12.0));
                });
            });
        });
    }

    // ------------------------------------------------------------------ channel table

    /// Rebuild the table's text fields from the specs (after loading, reordering, ⌘+scroll, …).
    fn sync_edits(&mut self) {
        self.edits = self.specs.iter().map(RowEdit::from_spec).collect();
    }

    fn preset(&self) -> Preset {
        let layout = (self.layout != preset::Layout::default()).then(|| self.layout.clone());
        Preset { time_base: Some(self.span), channels: self.specs.clone(), overview: self.overview.clone(), layout, labels: self.labels.clone() }
    }

    /// Open a file dialog (unless one is already open); its answer arrives in `dialog_answered`.
    fn open_dialog(&mut self, ctx: &egui::Context, what: FileDialog, d: rfd::FileDialog, save: bool) {
        if self.file_dialog.is_none() {
            self.file_dialog = Some(crate::dialog::open(ctx, what, move || if save { d.save_file() } else { d.pick_file() }));
        }
    }

    fn dialog_answered(&mut self, ctx: &egui::Context, what: FileDialog, path: Option<PathBuf>) {
        let Some(path) = path else { return };
        match what {
            FileDialog::LoadPreset => self.load_preset(path),
            FileDialog::SavePreset => self.save_preset(path),
            FileDialog::Video => self.attach_video(path, ctx),
            FileDialog::SaveClip(plan) => {
                if let Err(e) = self.run_export(ctx, plan, path) {
                    self.export.msg = Some((e, true));
                }
            }
        }
    }

    fn load_preset_dialog(&mut self, ctx: &egui::Context) {
        let mut d = rfd::FileDialog::new().set_title("Load channel preset").add_filter("JSON", &["json"]);
        if let Some(dir) = &self.last_dir {
            d = d.set_directory(dir);
        }
        self.open_dialog(ctx, FileDialog::LoadPreset, d, false);
    }

    fn load_preset(&mut self, path: PathBuf) {
        self.last_dir = path.parent().map(PathBuf::from);
        let parsed = std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|t| serde_json::from_str::<Preset>(&t).map_err(anyhow::Error::from));
        match parsed {
            Ok(p) => {
                self.overview = None;
                self.apply_preset(p);
                self.status = format!("loaded preset {}", path.display());
            }
            Err(e) => self.dialogs.push(("Could not load preset".into(), format!("{}:\n\n{e:#}", path.display()))),
        }
    }

    fn save_preset_dialog(&mut self, ctx: &egui::Context) {
        let mut d = rfd::FileDialog::new().set_title("Save channel preset").add_filter("JSON", &["json"]).set_file_name("preset.json");
        if let Some(dir) = &self.last_dir {
            d = d.set_directory(dir);
        }
        self.open_dialog(ctx, FileDialog::SavePreset, d, true);
    }

    fn save_preset(&mut self, mut path: PathBuf) {
        if path.extension().is_none_or(|e| e != "json") {
            path.set_extension("json");
        }
        self.last_dir = path.parent().map(PathBuf::from);
        let res = serde_json::to_string_pretty(&self.preset()).map_err(anyhow::Error::from)
            .and_then(|t| std::fs::write(&path, t + "\n").map_err(anyhow::Error::from));
        match res {
            Ok(()) => self.status = format!("saved preset {}", path.display()),
            Err(e) => self.dialogs.push(("Could not save preset".into(), format!("{}:\n\n{e:#}", path.display()))),
        }
    }

    fn choose_video_dialog(&mut self, ctx: &egui::Context) {
        let mut d = rfd::FileDialog::new().set_title("Video recorded during this session").add_filter("Video", &["mp4", "mkv", "mov", "avi"]);
        if let Some(dir) = self.cams.first().and_then(|c| c.dec.path.parent().map(PathBuf::from)).or(self.last_dir.clone()) {
            d = d.set_directory(dir);
        }
        self.open_dialog(ctx, FileDialog::Video, d, false);
    }

    /// The sidebar's tab bar: Channels | Events, and a button that collapses it (F2).
    fn sidebar_tabs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.side_tab, SideTab::Channels, "Channels").on_hover_text(
                "Low/High Hz: filter band (blank = none). Notch: e.g. 60 or 60,180.\n\
                 DSP wzrd: key=value for order, smooth_ms, env_lp, plot_fs, win_s.\n\
                 Y range: 'auto' or 'lo, hi'. Text fields apply on Enter or when you click away.\n\
                 ⌘/Ctrl+scroll over a trace scales its Y range; double-click a trace resets it to auto.\n\
                 Click a row's number to select it for Up / Down / Remove, and as the template for Add.",
            );
            ui.selectable_value(&mut self.side_tab, SideTab::Events, "Events").on_hover_text("the events marked in this recording");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("▶").on_hover_text("hide the sidebar (F2)").clicked() {
                    self.side_open = false;
                }
            });
        });
        ui.separator();
    }

    /// The Events tab (the list of marked events arrives with annotation).
    fn events_ui(&mut self, ui: &mut egui::Ui) {
        let name = self.ann.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if let Some(e) = &self.ann.load_error {
            ui.label(egui::RichText::new(format!("{name} couldn't be read, so nothing will be saved over it:\n{e}")).color(Color32::from_rgb(0xe0, 0x70, 0x60)));
            return;
        }
        let n = self.ann.events.len();
        ui.label(egui::RichText::new(if n == 0 { "No events yet.".to_string() } else { format!("{n} event{}", if n == 1 { "" } else { "s" }) }).color(LABEL))
            .on_hover_text(self.ann.path.display().to_string());
        ui.label(egui::RichText::new(format!("saved in {name}, next to the recording")).small().color(MUTED));
        ui.horizontal(|ui| {
            ui.label("annotator");
            ui.add(egui::TextEdit::singleline(&mut self.annotator).desired_width(90.0)).on_hover_text("written with each event you mark");
        });
        ui.separator();
        // which labels to list (and step through with Ctrl/Cmd+arrows)
        let shown = self.event_filter.clone().unwrap_or_else(|| "all labels".into());
        egui::ComboBox::from_id_salt("event_filter").selected_text(shown).show_ui(ui, |ui| {
            ui.selectable_value(&mut self.event_filter, None, "all labels");
            for l in &self.labels {
                ui.selectable_value(&mut self.event_filter, Some(l.name.clone()), &l.name);
            }
        });
        self.selected_event_ui(ui);
        let agreement = annotations::agreement(&self.ann.events);
        if !agreement.is_empty() && !self.audit {
            egui::CollapsingHeader::new("Agreement: video-only vs normal").default_open(false).show(ui, |ui| {
                for a in &agreement {
                    let ms = |d: Option<f64>| d.map(|s| format!("{:.0} ms", s * 1000.0)).unwrap_or_else(|| "–".into());
                    ui.label(egui::RichText::new(&a.label).color(self.label_color(&a.label)));
                    ui.label(
                        egui::RichText::new(format!(
                            "{} video-only, {} normal, {} matched\nonly video-only: {}   only normal: {}\nmean onset diff {}, offset diff {}\nover {} – {}",
                            a.audit, a.normal, a.matched, a.audit - a.matched, a.normal - a.matched,
                            ms(a.onset_diff), ms(a.offset_diff), fmt_time(a.span.0, None), fmt_time(a.span.1, None)
                        ))
                        .small()
                        .color(LABEL),
                    );
                }
                ui.label(
                    egui::RichText::new(format!(
                        "Events match when they overlap, or start within {:.0} ms; each pairs with the closest onset.",
                        annotations::MATCH_TOLERANCE_S * 1000.0
                    ))
                    .small()
                    .color(MUTED),
                );
            });
        }
        ui.separator();
        let mut jump = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for e in self.ann.events.iter().filter(|e| self.shows(e) && self.event_filter.as_ref().is_none_or(|f| *f == e.label)) {
                let len = if e.is_point() { String::new() } else { format!("  {:.2} s", e.end_s - e.start_s) };
                let color = self.label_color(&e.label);
                let r = ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("■").color(color));
                    let bright = self.flash.as_ref().is_some_and(|(id, at)| *id == e.id && at.elapsed().as_secs_f32() < 0.8);
                    ui.label(egui::RichText::new(&e.label).color(if bright { color } else { LABEL }));
                    ui.label(egui::RichText::new(format!("{}{len}", fmt_time(e.start_s, None))).monospace().small().color(MUTED));
                });
                let r = r.response.interact(Sense::click()).on_hover_text(if e.note.is_empty() { "click to select it and go there".to_string() } else { e.note.clone() });
                if self.selected_event.as_ref() == Some(&e.id) {
                    ui.painter().rect_stroke(r.rect.expand(1.0), 3.0, Stroke::new(1.0, color), egui::StrokeKind::Outside);
                    if self.list_scroll {
                        r.scroll_to_me(None);
                    }
                }
                if r.clicked() {
                    jump = Some(e.id.clone());
                }
            }
        });
        self.list_scroll = false;
        if let Some(id) = jump {
            self.select_event(&id);
        }
    }

    /// Whether an event is shown: in audit mode only the audit pass's own marks are.
    fn shows(&self, e: &annotations::Event) -> bool {
        !self.audit || e.view == "video"
    }

    /// Select an event (and go to its start).
    fn select_event(&mut self, id: &str) {
        let Some(e) = self.ann.get(id) else { return };
        let t = e.start_s;
        self.selected_event = Some(id.to_string());
        self.sync_note();
        self.set_time(t);
    }

    /// The note field follows the selected event.
    fn sync_note(&mut self) {
        self.note_edit = self.selected_event.as_deref().and_then(|id| self.ann.get(id)).map(|e| e.note.clone()).unwrap_or_default();
    }

    /// Ctrl/Cmd+arrow: the previous / next event (of the filtered label) from the cursor.
    fn step_event(&mut self, d: i64) {
        let t = self.t;
        let want = |e: &&annotations::Event| self.shows(e) && self.event_filter.as_ref().is_none_or(|f| *f == e.label);
        let hit = if d > 0 {
            self.ann.events.iter().filter(want).find(|e| e.start_s > t + 1e-6)
        } else {
            self.ann.events.iter().filter(want).rev().find(|e| e.start_s < t - 1e-6)
        };
        match hit.map(|e| e.id.clone()) {
            Some(id) => {
                self.select_event(&id);
                self.side_tab = SideTab::Events;
                self.list_scroll = true;
            }
            None => self.status = format!("no {} event {}", self.event_filter.as_deref().unwrap_or("other"), if d > 0 { "after this" } else { "before this" }),
        }
    }

    fn delete_selected(&mut self) {
        let Some(id) = self.selected_event.take() else { return };
        if let Some(e) = self.ann.remove(&id) {
            self.status = format!("removed {} at {} (Ctrl/Cmd+Z brings it back)", e.label, fmt_time(e.start_s, None));
            self.undo.push(Edit::Removed(e));
            self.redo.clear();
        }
        self.sync_note();
    }

    /// Apply a change to the selected event (undoable).
    fn change_selected(&mut self, f: impl FnOnce(&mut annotations::Event)) {
        let Some(before) = self.selected_event.as_deref().and_then(|id| self.ann.get(id)).cloned() else { return };
        let mut after = before.clone();
        f(&mut after);
        if after == before {
            return;
        }
        after.marked_at = annotations::now_utc();
        self.ann.replace(after.clone());
        self.undo.push(Edit::Changed(Box::new((before, after))));
        self.redo.clear();
    }

    /// The selected event: where it is, its note, its edges set to the current frame, Delete.
    fn selected_event_ui(&mut self, ui: &mut egui::Ui) {
        let Some(e) = self.selected_event.as_deref().and_then(|id| self.ann.get(id)).cloned() else { return };
        ui.add_space(4.0);
        let c = self.label_color(&e.label);
        let frames = match (e.frame_start, e.frame_end) {
            (Some(a), Some(b)) if a != b => format!("  frames {a}–{b}"),
            (Some(a), _) => format!("  frame {a}"),
            _ => String::new(),
        };
        let span = if e.is_point() { fmt_time(e.start_s, None) } else { format!("{} – {} ({:.2} s)", fmt_time(e.start_s, None), fmt_time(e.end_s, None), e.end_s - e.start_s) };
        ui.label(egui::RichText::new(format!("{}  {}", e.label, e.id)).color(c));
        ui.label(egui::RichText::new(format!("{span}{frames}")).small().monospace().color(LABEL));
        ui.label(egui::RichText::new(format!("{}  {}  {}", e.annotator, e.view, e.marked_at)).small().color(MUTED));
        let r = ui.add(egui::TextEdit::multiline(&mut self.note_edit).desired_rows(2).desired_width(f32::INFINITY).hint_text("note"));
        if r.lost_focus() {
            let note = self.note_edit.clone();
            self.change_selected(|e| e.note = note);
        }
        ui.horizontal_wrapped(|ui| {
            let here = self.mark_here();
            if !e.is_point() {
                if ui.button("Start = here").on_hover_text("move its start to the current frame").clicked() {
                    self.change_selected(|e| set_edge(e, here, true));
                }
                if ui.button("End = here").on_hover_text("move its end to the current frame").clicked() {
                    self.change_selected(|e| set_edge(e, here, false));
                }
            } else if ui.button("Move here").on_hover_text("move it to the current frame").clicked() {
                self.change_selected(|e| {
                    set_edge(e, here, true);
                    set_edge(e, here, false);
                });
            }
            if ui.button("Delete").on_hover_text("remove it (Delete key; Ctrl/Cmd+Z brings it back)").clicked() {
                self.delete_selected();
            }
        });
    }

    /// A label's colour (its own, else one from a palette by its place in the list).
    fn label_color(&self, name: &str) -> Color32 {
        const PALETTE: [&str; 6] = ["#e8b84a", "#5cc8a8", "#e8876a", "#a98be8", "#5aa8e8", "#c8c85a"];
        match self.labels.iter().position(|l| l.name == name) {
            Some(i) => self.labels[i].color.as_deref().map(hex).unwrap_or_else(|| hex(PALETTE[i % PALETTE.len()])),
            None => MUTED,
        }
    }

    /// Where a mark made now lands: the reference video's current frame (its trigger time and
    /// sample), or the cursor when there is no video.
    fn mark_here(&self) -> Mark {
        match self.cam0().and_then(|c| c.frame_at(self.t).map(|k| (c, k))) {
            Some((c, k)) => Mark { t: c.frames[k], sample: c.rows[k], frame: Some(k as i64) },
            None => Mark { t: self.t, sample: (self.t * self.rec.fs).round() as i64, frame: None },
        }
    }

    /// A label's key: mark a point now, or start / finish an interval.
    fn label_key(&mut self, n: usize) {
        if let Some(e) = &self.ann.load_error {
            self.status = format!("annotations are read-only: {e}");
            return;
        }
        let l = self.labels[n].clone();
        let here = self.mark_here();
        let (a, b) = match l.kind {
            preset::LabelKind::Point => (here, here),
            preset::LabelKind::Interval => match self.open_marks.iter().position(|(name, _)| *name == l.name) {
                None => {
                    self.status = format!("{} from {}: press {} again at its end (Esc cancels)", l.name, fmt_time(here.t, None), l.key);
                    self.open_marks.push((l.name, here));
                    return;
                }
                Some(i) => {
                    let start = self.open_marks.remove(i).1;
                    if start.t <= here.t { (start, here) } else { (here, start) }
                }
            },
        };
        let e = annotations::Event {
            id: String::new(),
            label: l.name.clone(),
            start_s: a.t,
            end_s: b.t,
            start_sample: a.sample,
            end_sample: b.sample,
            frame_start: a.frame,
            frame_end: b.frame,
            video: self.cam0().map(|c| c.name()).unwrap_or_default(),
            note: String::new(),
            annotator: self.annotator.trim().to_string(),
            view: if self.audit { "video".into() } else { "video+emg".into() },
            marked_at: annotations::now_utc(),
            kind: if l.kind == preset::LabelKind::Point { "point".into() } else { "interval".into() },
            extra: Default::default(),
        };
        let id = self.ann.add(e);
        let e = self.ann.get(&id).cloned().unwrap();
        self.status = if e.is_point() {
            format!("{} at {}", e.label, fmt_time(e.start_s, None))
        } else {
            format!("{} {} – {} ({:.2} s)", e.label, fmt_time(e.start_s, None), fmt_time(e.end_s, None), e.end_s - e.start_s)
        };
        self.flash = Some((id, Instant::now()));
        self.undo.push(Edit::Added(e));
        self.redo.clear();
    }

    fn undo_edit(&mut self) {
        let Some(op) = self.undo.pop() else { return };
        match &op {
            Edit::Added(e) => {
                self.ann.remove(&e.id);
                self.status = format!("undid: {} at {}", e.label, fmt_time(e.start_s, None));
            }
            Edit::Removed(e) => {
                self.ann.restore(e.clone());
                self.flash = Some((e.id.clone(), Instant::now()));
                self.status = format!("undid removing {} at {}", e.label, fmt_time(e.start_s, None));
            }
            Edit::Changed(c) => {
                let before = &c.0;
                self.ann.replace(before.clone());
                self.status = format!("undid the change to {} at {}", before.label, fmt_time(before.start_s, None));
            }
        }
        self.sync_note();
        self.redo.push(op);
    }

    fn redo_edit(&mut self) {
        let Some(op) = self.redo.pop() else { return };
        match &op {
            Edit::Added(e) => {
                self.ann.restore(e.clone());
                self.flash = Some((e.id.clone(), Instant::now()));
                self.status = format!("redid: {} at {}", e.label, fmt_time(e.start_s, None));
            }
            Edit::Removed(e) => {
                self.ann.remove(&e.id);
                self.status = format!("redid removing {} at {}", e.label, fmt_time(e.start_s, None));
            }
            Edit::Changed(c) => {
                let after = &c.1;
                self.ann.replace(after.clone());
                self.status = format!("redid the change to {} at {}", after.label, fmt_time(after.start_s, None));
            }
        }
        self.sync_note();
        self.undo.push(op);
    }

    /// Under the toolbar: every label with its key and colour; an open interval's chip is lit and
    /// says where it started.
    fn label_strip(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            for l in &self.labels {
                let c = self.label_color(&l.name);
                let open = self.open_marks.iter().find(|(n, _)| *n == l.name).map(|(_, m)| m.t);
                let text = match open {
                    Some(t) => format!("[{}] {}  since {}", l.key, l.name, fmt_time(t, None)),
                    None if l.key.is_empty() => l.name.clone(),
                    None => format!("[{}] {}", l.key, l.name),
                };
                let chip = egui::Frame::new()
                    .fill(if open.is_some() { c.gamma_multiply(0.35) } else { Color32::TRANSPARENT })
                    .stroke(Stroke::new(1.0, c.gamma_multiply(if open.is_some() { 1.0 } else { 0.5 })))
                    .corner_radius(4.0)
                    .inner_margin(egui::Margin::symmetric(6, 1));
                let kind = if l.kind == preset::LabelKind::Point { "a point" } else { "an interval: press once at its start and again at its end" };
                chip.show(ui, |ui| ui.label(egui::RichText::new(text).color(if open.is_some() { LABEL } else { c }).size(12.5)))
                    .response
                    .on_hover_text(format!("{}: {kind}", l.name));
            }
            let was = self.audit;
            ui.checkbox(&mut self.audit, "audit: video only").on_hover_text(
                "Score from the video alone: the traces and earlier marks are hidden, and events marked now are\n\
                 saved with view \"video\". Scoring a stretch both ways shows (Events tab) how much the traces\n\
                 shaped which events were called.",
            );
            if self.audit != was {
                self.selected_event = None;
                self.sync_note();
                self.status = if self.audit { "audit mode: traces and earlier marks hidden".into() } else { "audit mode off".into() };
            }
            if self.ann.load_error.is_some() {
                ui.label(egui::RichText::new("annotations read-only (see Events)").color(Color32::from_rgb(0xe0, 0x70, 0x60)));
            } else {
                let undo = if cfg!(target_os = "macos") { "Cmd+Z undo, Cmd+Shift+Z redo" } else { "Ctrl+Z undo, Ctrl+Shift+Z redo" };
                ui.label(egui::RichText::new(format!("Esc cancels an open interval · {undo}")).small().color(MUTED));
            }
        });
    }

    /// Save the annotations a second after the last change (and say so once if it fails).
    fn autosave(&mut self, after: std::time::Duration) {
        if self.ann.load_error.is_some() {
            return;
        }
        match self.ann.save_if_due(after) {
            Ok(_) => self.save_error = None,
            Err(e) => {
                let msg = format!("{e:#}");
                if self.save_error.as_ref() != Some(&msg) {
                    self.dialogs.push(("Couldn't save the annotations".into(), msg.clone()));
                    self.save_error = Some(msg);
                }
            }
        }
    }

    /// The collapsed sidebar: a thin rail that opens it again, on either tab.
    fn sidebar_rail(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(4.0);
            if ui.small_button("◀").on_hover_text("show the sidebar (F2)").clicked() {
                self.side_open = true;
            }
            ui.add_space(10.0);
            for (tab, letter, tip) in [(SideTab::Channels, "C", "Channels"), (SideTab::Events, "E", "Events")] {
                if ui.selectable_label(self.side_tab == tab, letter).on_hover_text(tip).clicked() {
                    self.side_tab = tab;
                    self.side_open = true;
                }
            }
        });
    }

    fn channels_ui(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        let mut action: Option<&str> = None;
        egui::Panel::bottom("channel_buttons").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for (text, tip) in [("Add", "add a row (a copy of the selected one)"), ("Remove", "remove the selected row"),
                                    ("Up", "move the selected row up"), ("Down", "move the selected row down"),
                                    ("Load…", "load a channel preset (.json)"), ("Save…", "save rows, time base and overview as a preset")] {
                    if ui.button(text).on_hover_text(tip).clicked() {
                        action = Some(text);
                    }
                }
            });
        });
        let names = self.rec.ch_names.clone();
        let mut refs = vec!["—".to_string()];
        refs.extend(names.iter().cloned());
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("channels").striped(true).spacing([4.0, 5.0]).show(ui, |ui| {
                for h in ["", "Show", "Label", "Ch", "Ref", "Mode", "Low Hz", "High Hz", "Notch", "DSP wzrd", "Y range", "Clip"] {
                    let l = ui.label(egui::RichText::new(h).small());
                    if h == "DSP wzrd" {
                        l.on_hover_text(
                            "key=value, space separated; blank = the mode's defaults.\n\
                             order      Butterworth order of the Low/High Hz band (all modes; 4 hilo/envelope, 2 slow/bandpower)\n\
                             smooth_ms  envelope: moving-average window after rectifying (20)\n\
                             env_lp     envelope: final low-pass, Hz (40)\n\
                             plot_fs    slow/envelope/bandpower: stored & drawn sample rate, Hz (1000/1000/10)\n\
                             win_s      bandpower: RMS window, s (30)",
                        );
                    } else if h == "Clip" {
                        l.on_hover_text(
                            "Where the trace is cut off when it leaves its Y range (its scale stays its row's):\n\
                             row    at its own row\n\
                             chart  at the edges of the chart, so it may cross other rows\n\
                             none   nowhere: it may also draw over the video and the overview strip",
                        );
                    }
                }
                ui.end_row();
                for i in 0..self.specs.len() {
                    if ui.selectable_label(self.selected == Some(i), format!("{:>2}", i + 1)).clicked() {
                        self.selected = if self.selected == Some(i) { None } else { Some(i) };
                    }
                    let s = &mut self.specs[i];
                    let e = &mut self.edits[i];
                    changed |= ui.checkbox(&mut s.show, "").changed();
                    let mut label = s.label.clone().unwrap_or_default();
                    if ui.add_sized([92.0, 20.0], egui::TextEdit::singleline(&mut label)).changed() {
                        s.label = Some(label);
                        changed = true;
                    }
                    egui::ComboBox::from_id_salt(("ch", i)).width(54.0).selected_text(&s.ch).show_ui(ui, |ui| {
                        for n in &names {
                            changed |= ui.selectable_value(&mut s.ch, n.clone(), n).changed();
                        }
                    });
                    let mut r = s.reference.clone().unwrap_or_else(|| "—".into());
                    egui::ComboBox::from_id_salt(("ref", i)).width(54.0).selected_text(&r).show_ui(ui, |ui| {
                        for n in &refs {
                            if ui.selectable_value(&mut r, n.clone(), n).changed() {
                                changed = true;
                            }
                        }
                    });
                    s.reference = (r != "—").then_some(r);
                    let before = s.mode.clone();
                    egui::ComboBox::from_id_salt(("mode", i)).width(78.0).selected_text(&s.mode).show_ui(ui, |ui| {
                        for m in MODES {
                            ui.selectable_value(&mut s.mode, m.to_string(), m);
                        }
                    });
                    if s.mode != before {
                        // band, order etc. belong to the mode: start the new mode from its defaults
                        s.extra.remove("band");
                        for k in EXTRA_KEYS {
                            s.extra.remove(k);
                        }
                        s.ylim = None;
                        *e = RowEdit::from_spec(s);
                        changed = true;
                    }
                    let mut commit = false;
                    for (buf, w) in [(&mut e.lo, 42.0), (&mut e.hi, 42.0), (&mut e.notch, 50.0), (&mut e.extra, 72.0), (&mut e.y, 66.0)] {
                        let resp = ui.add_sized([w, 20.0], egui::TextEdit::singleline(buf));
                        commit |= resp.lost_focus();
                    }
                    if commit {
                        let before = s.clone();
                        e.apply(s);
                        *e = RowEdit::from_spec(s); // show what was understood (invalid input reverts)
                        changed |= *s != before;
                    }
                    changed |= clip_switch(ui, &mut s.clip);
                    ui.end_row();
                }
            });
        });
        match action {
            Some("Add") => {
                let mut s = self.selected.and_then(|i| self.specs.get(i).cloned()).unwrap_or_else(|| Spec::new(&names[0], "slow"));
                s.ylim = None;
                s.show = true;
                self.specs.push(s);
                self.selected = Some(self.specs.len() - 1);
                changed = true;
            }
            Some("Remove") => {
                if let Some(i) = self.selected.filter(|i| *i < self.specs.len()) {
                    self.specs.remove(i);
                    self.selected = None;
                    changed = true;
                }
            }
            Some(dir @ ("Up" | "Down")) => {
                if let Some(i) = self.selected {
                    let j = if dir == "Up" { i.checked_sub(1) } else { Some(i + 1).filter(|j| *j < self.specs.len()) };
                    if let Some(j) = j {
                        self.specs.swap(i, j);
                        self.selected = Some(j);
                        changed = true;
                    }
                }
            }
            Some("Load…") => self.load_preset_dialog(ui.ctx()),
            Some("Save…") => self.save_preset_dialog(ui.ctx()),
            _ => {}
        }
        if changed {
            self.sync_edits();
            self.set_specs();
        }
    }

    /// The chart (rows, axes, cursor) in `rect`, over whatever is already painted there. `resp`
    /// is its click/drag response; `pointer_in`: the wheel and hover act on it.
    fn traces_ui(&mut self, ui: &mut egui::Ui, rect: Rect, resp: &egui::Response, pointer_in: bool) {
        let painter = ui.painter_at(rect);
        let ppp = ui.ctx().pixels_per_point();
        let x0 = rect.left() + MARGIN_X + LABEL_W + YAXIS_W;
        let x1 = rect.right() - MARGIN_X;
        let top = rect.top() + 4.0;
        let bottom = rect.bottom() - 2.0 - TAXIS_H;
        let n = self.rows.len();
        if n == 0 || x1 <= x0 + 10.0 || bottom <= top + 10.0 {
            painter.text(rect.center(), Align2::CENTER_CENTER, "no channels shown (tick some in the Channels panel)", FontId::proportional(13.0), MUTED);
            return;
        }
        let row_h = ((bottom - top) - ROW_GAP * (n as f32 - 1.0)) / n as f32;
        let plot_w = x1 - x0;
        let (t, span) = (self.t, self.span);
        let (t_lo, t_hi) = (t - span / 2.0, t + span / 2.0);
        let mut need_window = vec![];
        let font = FontId::proportional(12.0);
        let small = FontId::proportional(11.0);
        let mut xs = Vec::new();
        let mut ys = Vec::new();
        // where traces that aren't clipped to their row may draw (always within the time axis)
        let chart = Rect::from_min_max(Pos2::new(x0, top), Pos2::new(x1, bottom));
        let free = if self.free_area.is_positive() {
            Rect::from_x_y_ranges(x0..=x1, self.free_area.top()..=self.free_area.bottom())
        } else {
            chart
        };
        for i in 0..n {
            let r = Rect::from_min_max(Pos2::new(x0, top + i as f32 * (row_h + ROW_GAP)), Pos2::new(x1, top + i as f32 * (row_h + ROW_GAP) + row_h));
            painter.text(Pos2::new(x0 - YAXIS_W - 4.0, r.center().y), Align2::RIGHT_CENTER, self.rows[i].spec.title(), font.clone(), LABEL);
            let tr = self.trace_for(&self.rows[i]);
            let (lo, hi) = self.row_ylim(i, tr.as_deref());
            // y axis: few, round ticks so stacked rows' labels don't collide
            let ax = x0 - 2.0;
            painter.line_segment([Pos2::new(ax, r.top()), Pos2::new(ax, r.bottom())], Stroke::new(1.0, FG));
            let ymap = |v: f64| r.bottom() - ((v - lo) / (hi - lo)) as f32 * r.height();
            let ticks: Vec<(f64, f64)> = if r.height() < 50.0 {
                if lo <= 0.0 && 0.0 <= hi { vec![(0.0, 1.0)] } else { vec![] }
            } else {
                let target = 0.95 * (hi - lo) / 2.0;
                let e = 10f64.powf(target.log10().floor());
                let step = [1.0, 1.5, 2.0, 2.5, 3.0, 4.0, 5.0, 6.0, 8.0].iter().map(|m| m * e).filter(|s| *s <= target).fold(e, f64::max);
                let k0 = (lo / step).ceil() as i64;
                let k1 = (hi / step).floor() as i64;
                (k0..=k1).map(|k| (k as f64 * step, step)).collect()
            };
            for (v, step) in ticks {
                let y = ymap(v);
                painter.line_segment([Pos2::new(ax - 4.0, y), Pos2::new(ax, y)], Stroke::new(1.0, FG));
                painter.text(Pos2::new(ax - 6.0, y), Align2::RIGHT_CENTER, fmt_tick(v, step), small.clone(), FG);
            }
            match tr {
                None => {
                    let msg = if span <= WINDOW_MAX_S { "filtering…" } else { "filtering session… (zoom in to preview)" };
                    painter.text(r.center(), Align2::CENTER_CENTER, msg, small.clone(), MUTED);
                    if span <= WINDOW_MAX_S {
                        need_window.push(self.rows[i].spec.clone());
                    }
                }
                Some(_) if self.audit => {
                    painter.text(r.center(), Align2::CENTER_CENTER, "hidden (audit: video only)", small.clone(), MUTED);
                }
                Some(tr) => {
                    xs.clear();
                    ys.clear();
                    tr.view(t_lo - 0.01 * span, t_hi + 0.01 * span, (plot_w * ppp) as f64 * 1.02, &mut xs, &mut ys);
                    // Drawn in `area` (the row, or more of the window), scaled to the row: y in
                    // screen points, then in the area's normalized device coordinates.
                    let area = match self.rows[i].spec.clip {
                        Clip::Row => r.intersect(rect),
                        Clip::Chart => chart,
                        Clip::Unclipped => free,
                    };
                    let (r_bottom, r_h, a_top, a_h) = (r.bottom() as f64, r.height() as f64, area.top() as f64, area.height().max(1.0) as f64);
                    let points: Vec<[f32; 2]> = xs
                        .iter()
                        .zip(&ys)
                        .map(|(x, y)| {
                            let sy = r_bottom - (*y as f64 - lo) / (hi - lo) * r_h;
                            [((x - t) / (span / 2.0)) as f32, (1.0 - 2.0 * (sy - a_top) / a_h) as f32]
                        })
                        .collect();
                    let outline = self.layout.outline.then_some(self.layout.outline_px);
                    let cb = LineStrip { slot: i, points, color: self.rows[i].color, px: px_for(area, ppp), outline };
                    // the layer's own painter, so the clip can reach past this panel (video, overview)
                    ui.ctx().layer_painter(ui.layer_id()).with_clip_rect(area).add(egui_wgpu::Callback::new_paint_callback(area, cb));
                }
            }
        }
        // annotated events: bands for intervals, ticks for points; open intervals run to the cursor
        let ev_painter = ui.painter_at(Rect::from_min_max(Pos2::new(x0, top), Pos2::new(x1, bottom)));
        let tx_abs = |v: f64| x0 + ((v - t_lo) / span) as f32 * plot_w;
        for e in self.ann.events.iter().filter(|e| self.shows(e)) {
            if e.end_s < t_lo || e.start_s > t_hi {
                continue;
            }
            let c = self.label_color(&e.label);
            let bright = self.flash.as_ref().is_some_and(|(id, at)| *id == e.id && at.elapsed().as_secs_f32() < 0.8);
            let selected = self.selected_event.as_ref() == Some(&e.id);
            if e.is_point() {
                let x = tx_abs(e.start_s);
                ev_painter.line_segment([Pos2::new(x, top), Pos2::new(x, bottom)], Stroke::new(if bright || selected { 3.0 } else { 1.5 }, c));
            } else {
                let (a, b) = (tx_abs(e.start_s), tx_abs(e.end_s).max(tx_abs(e.start_s) + 2.0));
                let band = Rect::from_min_max(Pos2::new(a, top), Pos2::new(b, bottom));
                ev_painter.rect_filled(band, 0.0, c.gamma_multiply(if bright { 0.45 } else { 0.16 }));
                ev_painter.rect_filled(Rect::from_min_max(band.min, Pos2::new(b, top + 3.0)), 0.0, c);
                if selected {
                    ev_painter.rect_stroke(band, 0.0, Stroke::new(1.5, c), egui::StrokeKind::Inside);
                }
                if b - a > 40.0 {
                    ev_painter.text(Pos2::new(a + 3.0, top + 4.0), Align2::LEFT_TOP, &e.label, FontId::proportional(10.5), c);
                }
            }
        }
        for (label, m) in &self.open_marks {
            let c = self.label_color(label);
            let (a, b) = (tx_abs(m.t.min(t)), tx_abs(m.t.max(t)));
            let band = Rect::from_min_max(Pos2::new(a, top), Pos2::new(b.max(a + 2.0), bottom));
            ev_painter.rect_filled(band, 0.0, c.gamma_multiply(0.25));
            ev_painter.rect_stroke(band, 0.0, Stroke::new(1.5, c), egui::StrokeKind::Inside);
        }
        if self.flash.as_ref().is_some_and(|(_, at)| at.elapsed().as_secs_f32() < 0.8) {
            ui.ctx().request_repaint();
        }
        // cursor line and time axis (time relative to the cursor; signed tick labels)
        let cx = x0 + plot_w / 2.0;
        let fg_painter = ui.painter_at(rect);
        fg_painter.line_segment([Pos2::new(cx, top), Pos2::new(cx, bottom)], Stroke::new(1.0, Color32::WHITE));
        fg_painter.line_segment([Pos2::new(x0, bottom + 1.0), Pos2::new(x1, bottom + 1.0)], Stroke::new(1.0, FG));
        let (major, minor) = nice_spacing(span, (plot_w as f64 / 110.0).floor().max(3.0));
        let tx = |v: f64| x0 + ((v + span / 2.0) / span) as f32 * plot_w;
        for (step, len, labelled) in [(minor, 3.0, false), (major, 5.0, true)] {
            let mut k = (-span / 2.0 / step).ceil() as i64;
            while (k as f64) * step <= span / 2.0 {
                let v = k as f64 * step;
                fg_painter.line_segment([Pos2::new(tx(v), bottom + 1.0), Pos2::new(tx(v), bottom + 1.0 + len)], Stroke::new(1.0, FG));
                if labelled {
                    let label = if v.abs() < major * 1e-3 {
                        "0".to_string()
                    } else if major >= 60.0 {
                        format!("{}{}", if v > 0.0 { "+" } else { "" }, fmt_time(v, Some(major * 10.0)))
                    } else {
                        let d = (-(major.log10().floor()) as i32).max(0) as usize;
                        format!("{v:+.d$}")
                    };
                    fg_painter.text(Pos2::new(tx(v), bottom + 6.0), Align2::CENTER_TOP, label, small.clone(), FG);
                }
                k += 1;
            }
        }
        // on-demand processing of the visible window for rows that aren't cached yet
        if !need_window.is_empty() {
            self.window_worker.request(need_window, t_lo - span, t_hi + span, &self.cache);
        }
        // mouse
        // (also over a floating box in arrange mode, which takes the drags)
        let hover_pos = if pointer_in { ui.ctx().pointer_hover_pos() } else { None };
        let hover_row = hover_pos.and_then(|p| {
            let i = ((p.y - top) / (row_h + ROW_GAP)).floor();
            (i >= 0.0 && (i as usize) < n).then_some(i as usize)
        });
        if pointer_in {
            self.wheel(ui.ctx(), hover_row, plot_w);
        }
        if resp.dragged_by(egui::PointerButton::Primary) {
            self.set_time(self.t - resp.drag_delta().x as f64 * self.span / plot_w as f64);
        }
        if resp.double_clicked() {
            if let Some(i) = hover_row {
                self.reset_row_y(i);
            }
        }
    }

    fn overview_ui(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0.0, BG);
        let ppp = ui.ctx().pixels_per_point();
        let x0 = rect.left() + 70.0;
        let x1 = rect.right() - MARGIN_X;
        let top = rect.top() + 6.0;
        let bottom = rect.bottom() - TAXIS_H - 2.0;
        let plot = Rect::from_min_max(Pos2::new(x0, top), Pos2::new(x1, bottom));
        let dur = self.rec.duration();
        let tx = |v: f64| x0 + (v / dur) as f32 * (x1 - x0);
        let small = FontId::proportional(11.0);
        p.line_segment([Pos2::new(x0 - 1.0, top), Pos2::new(x0 - 1.0, bottom)], Stroke::new(1.0, FG));
        p.line_segment([Pos2::new(x0, bottom + 1.0), Pos2::new(x1, bottom + 1.0)], Stroke::new(1.0, FG));
        let title = self.overview.as_ref().map(|o| o.title()).unwrap_or_default();
        let tr = self.overview.as_ref().and_then(|o| self.cache.get(o));
        match tr {
            Some(tr) if plot.width() > 10.0 => {
                let (mut xs, mut ys) = (vec![], vec![]);
                tr.view(0.0, dur, (plot.width() * ppp) as f64, &mut xs, &mut ys);
                let finite = ys.iter().filter(|v| v.is_finite());
                let lo = finite.clone().fold(f32::INFINITY, |a, b| a.min(*b)) as f64;
                let hi = finite.fold(f32::NEG_INFINITY, |a, b| a.max(*b)) as f64;
                if lo.is_finite() && hi > lo {
                    let pad = 0.05 * (hi - lo);
                    let (lo, hi) = (lo - pad, hi + pad);
                    let points = xs
                        .iter()
                        .zip(&ys)
                        .map(|(x, y)| [((x / dur) * 2.0 - 1.0) as f32, (((*y as f64 - lo) / (hi - lo)) * 2.0 - 1.0) as f32])
                        .collect();
                    let cb = LineStrip { slot: OVERVIEW_SLOT, points, color: FG, px: px_for(plot, ppp), outline: None };
                    if !self.audit {
                        ui.painter().with_clip_rect(plot).add(egui_wgpu::Callback::new_paint_callback(plot, cb));
                    }
                    // a few y labels
                    let target = (hi - lo) / 2.0;
                    let e = 10f64.powf(target.log10().floor());
                    let step = [1.0, 2.0, 2.5, 5.0].iter().map(|m| m * e).filter(|s| *s <= target).fold(e, f64::max);
                    let mut k = (lo / step).ceil() as i64;
                    while k as f64 * step <= hi {
                        let v = k as f64 * step;
                        let y = bottom - ((v - lo) / (hi - lo)) as f32 * plot.height();
                        p.text(Pos2::new(x0 - 5.0, y), Align2::RIGHT_CENTER, fmt_tick(v, step), small.clone(), FG);
                        k += 1;
                    }
                }
            }
            _ => {
                p.text(plot.center(), Align2::CENTER_CENTER, if self.overview.is_some() { "filtering…" } else { "" }, small.clone(), MUTED);
            }
        }
        p.text(Pos2::new(x0 + 4.0, top + 2.0), Align2::LEFT_TOP, title, FontId::proportional(12.0), FG);
        // view region and cursor
        let (a, b) = (tx(self.t - self.span / 2.0).max(x0), tx(self.t + self.span / 2.0).min(x1));
        p.rect_filled(Rect::from_min_max(Pos2::new(a, top), Pos2::new(b.max(a + 1.0), bottom)), 0.0, Color32::from_white_alpha(40));
        p.line_segment([Pos2::new(tx(self.t), top), Pos2::new(tx(self.t), bottom)], Stroke::new(1.0, Color32::WHITE));
        // time axis
        // annotated events along the bottom of the strip
        for e in self.ann.events.iter().filter(|e| self.shows(e)) {
            let (a, b) = (tx(e.start_s), tx(e.end_s).max(tx(e.start_s) + 1.5));
            p.rect_filled(Rect::from_min_max(Pos2::new(a, bottom - 7.0), Pos2::new(b, bottom - 1.0)), 0.0, self.label_color(&e.label));
        }
        let (major, _) = nice_spacing(dur, ((x1 - x0) as f64 / 110.0).floor().max(3.0));
        let mut k = 1;
        while k as f64 * major < dur {
            let v = k as f64 * major;
            p.line_segment([Pos2::new(tx(v), bottom + 1.0), Pos2::new(tx(v), bottom + 6.0)], Stroke::new(1.0, FG));
            p.text(Pos2::new(tx(v), bottom + 6.0), Align2::CENTER_TOP, fmt_time(v, Some(major * 10.0)), small.clone(), FG);
            k += 1;
        }
        if resp.clicked() || resp.dragged_by(egui::PointerButton::Primary) {
            if let Some(pos) = resp.interact_pointer_pos() {
                self.set_time(((pos.x - x0) / (x1 - x0)) as f64 * dur);
            }
        }
    }

    fn dialogs_ui(&mut self, ctx: &egui::Context) {
        let mut close = None;
        for (i, (title, body)) in self.dialogs.iter().enumerate() {
            egui::Window::new(title.as_str()).id(egui::Id::new(("dialog", i))).collapsible(false).resizable(false)
                .default_width(460.0).anchor(Align2::CENTER_CENTER, Vec2::new(0.0, i as f32 * 24.0))
                .show(ctx, |ui| {
                    ui.label(body.as_str());
                    if ui.button("OK").clicked() {
                        close = Some(i);
                    }
                });
        }
        if let Some(i) = close {
            self.dialogs.remove(i);
        }
    }
}

/// The "Export clip" dialog and a running export.
#[derive(Default)]
struct Export {
    open: bool,
    from: String,
    to: String,
    time_base: String,
    width: String,
    speed: String,
    msg: Option<(String, bool)>,
    run: Option<ExportRun>,
}

struct ExportRun {
    progress: Arc<std::sync::Mutex<f64>>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    rx: std::sync::mpsc::Receiver<Result<(PathBuf, crate::clip::ClipDone), String>>,
    started: Instant,
}

/// Seconds, or m:ss / h:mm:ss.
fn parse_clock(s: &str) -> Option<f64> {
    let parts: Option<Vec<f64>> = s.trim().split(':').map(|p| p.trim().parse().ok()).collect();
    let parts = parts?;
    Some(parts.iter().rev().enumerate().map(|(i, v)| v * 60f64.powi(i as i32)).sum())
}

impl App {
    /// The reference video's first and last trigger times (the whole recording without one).
    fn video_span(&self) -> (f64, f64) {
        match self.cam0() {
            Some(c) => (c.frames[0], *c.frames.last().unwrap()),
            None => (0.0, self.rec.duration()),
        }
    }

    fn open_export(&mut self) {
        let (first, last) = self.video_span();
        let e = &mut self.export;
        if e.run.is_none() {
            let (a, b) = (self.t - self.span / 2.0, self.t + self.span / 2.0);
            e.from = fmt_time(a.max(first), None);
            e.to = fmt_time(b.min(last), None);
            if e.time_base.is_empty() {
                e.time_base = g3(self.span.min(10.0));
            }
            if e.speed.is_empty() {
                e.speed = "1".into();
            }
            e.msg = None;
        }
        e.open = true;
    }

    fn export_ui(&mut self, ctx: &egui::Context) {
        // a running export: progress, or its result
        if let Some(run) = &self.export.run {
            match run.rx.try_recv() {
                Ok(Ok((path, d))) => {
                    let secs = run.started.elapsed().as_secs_f64();
                    self.export.msg = Some((
                        format!(
                            "Wrote {} ({} frames, {}×{}, {:.3} frames/s, {}) in {:.0} s.",
                            path.display(), d.frames, d.size.0, d.size.1, d.fps, d.codec, secs
                        ),
                        false,
                    ));
                    self.status = format!("exported {}", path.display());
                    self.export.run = None;
                }
                Ok(Err(e)) => {
                    self.export.msg = Some((e, true));
                    self.export.run = None;
                }
                Err(_) => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
        if !self.export.open {
            return;
        }
        let mut open = true;
        let mut start = false;
        egui::Window::new("Export clip").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            let running = self.export.run.is_some();
            ui.add_enabled_ui(!running, |ui| {
                egui::Grid::new("export-grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    let e = &mut self.export;
                    ui.label("From");
                    ui.add(egui::TextEdit::singleline(&mut e.from).desired_width(110.0).hint_text("h:mm:ss"));
                    ui.end_row();
                    ui.label("To");
                    ui.add(egui::TextEdit::singleline(&mut e.to).desired_width(110.0).hint_text("h:mm:ss"));
                    ui.end_row();
                    ui.label("Time base (s)").on_hover_text("seconds of data across the chart: half before, half after each frame");
                    ui.add(egui::TextEdit::singleline(&mut e.time_base).desired_width(110.0));
                    ui.end_row();
                    ui.label("Width (px)").on_hover_text("blank: the video's own width");
                    ui.add(egui::TextEdit::singleline(&mut e.width).desired_width(110.0).hint_text("video's"));
                    ui.end_row();
                    ui.label("Speed (×)").on_hover_text("1 = real time; 0.25 = four times slower");
                    ui.add(egui::TextEdit::singleline(&mut e.speed).desired_width(110.0));
                    ui.end_row();
                });
                if ui.small_button("Use the current view").clicked() {
                    let (a, b) = (self.t - self.span / 2.0, self.t + self.span / 2.0);
                    let (first, last) = self.video_span();
                    self.export.from = fmt_time(a.max(first), None);
                    self.export.to = fmt_time(b.min(last), None);
                }
                ui.label(
                    egui::RichText::new(format!(
                        "The {} rows shown, with their colours and Y ranges, under the video.",
                        self.rows.len()
                    ))
                    .color(MUTED)
                    .size(12.0),
                );
            });
            ui.add_space(6.0);
            if let Some(run) = &self.export.run {
                let f = *run.progress.lock().unwrap();
                ui.add(egui::ProgressBar::new(f as f32).desired_width(320.0).text(format!("exporting… {:.0}%", f * 100.0)));
                if ui.button("Cancel").clicked() {
                    run.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            } else if ui.button("Export…").clicked() {
                start = true;
            }
            if let Some((m, err)) = &self.export.msg {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(m).color(if *err { Color32::from_rgb(0xe0, 0x70, 0x60) } else { FG }));
            }
        });
        self.export.open = open || self.export.run.is_some();
        if start
            && let Err(e) = self.start_export(ctx)
        {
            self.export.msg = Some((e, true));
        }
    }

    fn start_export(&mut self, ctx: &egui::Context) -> Result<(), String> {
        let e = &self.export;
        let from = parse_clock(&e.from).ok_or("From isn't a time (seconds or h:mm:ss)")?;
        let to = parse_clock(&e.to).ok_or("To isn't a time (seconds or h:mm:ss)")?;
        let time_base: f64 = e.time_base.trim().parse().map_err(|_| "Time base isn't a number")?;
        let speed: f64 = e.speed.trim().trim_end_matches(['x', '×']).parse().map_err(|_| "Speed isn't a number")?;
        let width: Option<u32> = if e.width.trim().is_empty() { None } else { Some(e.width.trim().parse().map_err(|_| "Width isn't a whole number")?) };
        if to <= from {
            return Err("To must be after From".into());
        }
        if speed <= 0.0 || time_base <= 0.0 {
            return Err("Speed and time base must be positive".into());
        }
        let video = self.cam0().map(|c| c.dec.path.clone()).ok_or("no video")?;
        let stem = video.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let name = format!("{stem}_{}-{}.mp4", fmt_time(from, Some(1e9)).replace(':', "-"), fmt_time(to, Some(1e9)).replace(':', "-"));
        let mut d = rfd::FileDialog::new().set_title("Save clip").add_filter("MP4 video", &["mp4"]).set_file_name(&name);
        if let Some(dir) = video.parent() {
            d = d.set_directory(dir);
        }
        let plan = ClipPlan { video, from, to, time_base, width, speed };
        self.open_dialog(ctx, FileDialog::SaveClip(plan), d, true);
        Ok(())
    }

    /// Render the checked clip to `out`, on a worker thread.
    fn run_export(&mut self, ctx: &egui::Context, plan: ClipPlan, mut out: PathBuf) -> Result<(), String> {
        let ClipPlan { video, from, to, time_base, width, speed } = plan;
        if out.extension().is_none() {
            out.set_extension("mp4");
        }
        // the rows as shown: their colours and current Y ranges
        let mut rows = vec![];
        for i in 0..self.rows.len() {
            let tr = self.trace_for(&self.rows[i]);
            let (lo, hi) = self.row_ylim(i, tr.as_deref());
            let r = &self.rows[i];
            let unit = self.rec.ch(&r.spec.ch).map(|c| self.rec.units[c].replace("uV", "µV")).unwrap_or_default();
            rows.push(crate::clip::ClipRow {
                spec: r.spec.clone(),
                label: r.spec.label.clone().unwrap_or_else(|| r.spec.ch.clone()),
                unit,
                color: [r.color.r(), r.color.g(), r.color.b()],
                ylim: Some((lo, hi)),
            });
        }
        let job = crate::clip::ClipJob { out: out.clone(), video, from, to, time_base, width, speed, rows };
        let (rec, cache) = (self.rec.clone(), self.cache.clone());
        let (frames, period) = self.cam0().map(|c| (c.rows.clone(), c.period)).ok_or("no video")?;
        let index_dir = self.cache.root.join("video_index");
        let progress = Arc::new(std::sync::Mutex::new(0.0));
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let (p2, c2, ctx2) = (progress.clone(), cancel.clone(), ctx.clone());
        std::thread::Builder::new()
            .name("clip-export".into())
            .spawn(move || {
                let r = crate::clip::render(
                    &job,
                    &rec,
                    &cache,
                    &frames,
                    period,
                    &index_dir,
                    &|f| {
                        *p2.lock().unwrap() = f;
                        ctx2.request_repaint();
                    },
                    &|| c2.load(std::sync::atomic::Ordering::Relaxed),
                );
                let _ = tx.send(r.map(|d| (out, d)).map_err(|e| format!("Export failed: {e:#}")));
                ctx2.request_repaint();
            })
            .map_err(|e| e.to_string())?;
        self.export.run = Some(ExportRun { progress, cancel, rx, started: Instant::now() });
        self.export.msg = None;
        Ok(())
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if let Some((last, times)) = &mut self.fps {
            times.push(ctx.input(|i| i.unstable_dt) as f64 * 1000.0);
            if last.elapsed().as_secs_f64() >= 1.0 {
                times.sort_by(|a, b| a.total_cmp(b));
                let n = times.len();
                eprintln!("fps {n:4}  frame ms median {:.1}  95% {:.1}  max {:.1}", times[n / 2], times[n * 95 / 100], times[n - 1]);
                *last = Instant::now();
                times.clear();
            }
        }
        self.tick();
        // F2 shows or hides the sidebar, even while a text field has focus
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F2)) {
            self.side_open = !self.side_open;
        }
        self.autosave(std::time::Duration::from_secs(1));
        if let Some((what, path)) = crate::dialog::take_answer(&mut self.file_dialog) {
            self.dialog_answered(&ctx, what, path);
        }
        self.poll_video(&ctx);
        self.keys(&ctx);
        let building = self.builder.progress.lock().unwrap().is_some();
        if self.was_building && !building {
            for r in &mut self.rows {
                r.auto = None; // switch from window-based to session-based auto ranges
            }
        }
        self.was_building = building;

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(3.0);
            self.toolbar(ui);
            ui.add_space(3.0);
        });
        if !self.labels.is_empty() {
            egui::Panel::top("labels").show(ui, |ui| self.label_strip(ui));
        }
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        let overview = egui::Panel::bottom("overview").exact_size(110.0).frame(egui::Frame::NONE.fill(BG).inner_margin(4.0)).show(ui, |ui| self.overview_ui(ui)).response.rect;
        let max_w = (ui.available_width() * 0.45).max(120.0);
        if self.side_open {
            // each tab keeps its own width: the channel table is wide, the events list narrow
            let (id, width) = match self.side_tab {
                SideTab::Channels => ("channels", 760.0f32),
                SideTab::Events => ("events", 260.0),
            };
            egui::Panel::right(id).resizable(true).default_size(width.min(max_w)).max_size(max_w).show(ui, |ui| {
                self.sidebar_tabs(ui);
                match self.side_tab {
                    SideTab::Channels => self.channels_ui(ui),
                    SideTab::Events => self.events_ui(ui),
                }
            });
        } else {
            egui::Panel::right("sidebar_rail").exact_size(30.0).resizable(false).show(ui, |ui| self.sidebar_rail(ui));
        }
        egui::CentralPanel::no_frame().frame(egui::Frame::NONE.fill(BG).inner_margin(4.0)).show(ui, |ui| {
            let central = ui.max_rect();
            self.free_area = Rect::from_min_max(central.min, Pos2::new(central.right(), overview.bottom().max(central.bottom())));
            if self.floating() {
                self.view_ui(ui);
                return;
            }
            if self.has_video() {
                let h = ui.available_height();
                egui::Panel::top("video")
                    .resizable(true)
                    .default_size(h * 0.45)
                    .size_range(60.0..=h - 80.0)
                    .frame(egui::Frame::NONE.fill(Color32::BLACK))
                    .show(ui, |ui| self.video_ui(ui));
            }
            let rect = ui.available_rect_before_wrap();
            let resp = ui.allocate_rect(rect, Sense::click_and_drag());
            ui.painter_at(rect).rect_filled(rect, 0.0, BG);
            let hovered = resp.hovered();
            self.traces_ui(ui, rect, &resp, hovered);
        });
        self.dialogs_ui(&ctx);
        // (testing: SYNCVIEW_RS_TEST_EXPORT=1 opens the export dialog once the video is synced)
        if self.cam0().is_some() && !self.export.open && self.export.from.is_empty() && std::env::var_os("SYNCVIEW_RS_TEST_EXPORT").is_some() {
            self.open_export();
        }
        self.export_ui(&ctx);
        if self.playing {
            ctx.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> Spec {
        serde_json::from_value(json!({"ch": "CH5", "ref": "CH6", "mode": "hilo", "label": "x",
            "band": [100.0, 3000.0], "notch": [60.0, 120.0], "order": 3, "ylim": [-50.0, 50.0]})).unwrap()
    }

    #[test]
    fn times_round_into_the_next_minute() {
        assert_eq!(fmt_time(419.98, Some(3600.0)), "7:00");
        assert_eq!(fmt_time(119.96, Some(60.0)), "2:00.0");
        assert_eq!(fmt_time(3599.9996, None), "1:00:00.000");
        assert_eq!(fmt_time(61.3, Some(60.0)), "1:01.3");
        assert_eq!(fmt_time(-5.0, None), "-0:05.000");
    }

    #[test]
    fn table_fields_round_trip() {
        let s = spec();
        let e = RowEdit::from_spec(&s);
        assert_eq!((e.lo.as_str(), e.hi.as_str(), e.notch.as_str(), e.extra.as_str(), e.y.as_str()),
                   ("100", "3000", "60,120", "order=3", "-50, 50"));
        let mut t = s.clone();
        e.apply(&mut t);
        assert_eq!(t, s, "unchanged fields must not change the spec (or its cache key)");
    }

    #[test]
    fn table_edits() {
        let mut s = spec();
        let mut e = RowEdit::from_spec(&s);
        e.lo = "200".into();
        e.hi = "".into(); // blank = no low-pass
        e.notch = "50".into();
        e.extra = "order=2 smooth_ms=5 bogus=1".into();
        e.y = "auto".into();
        e.apply(&mut s);
        assert_eq!(s.extra["band"], json!([200.0, null]));
        assert_eq!(s.extra["notch"], json!([50.0]));
        assert_eq!((s.extra["order"].clone(), s.extra["smooth_ms"].clone()), (json!(2), json!(5.0)));
        assert!(s.extra.get("bogus").is_none() && s.ylim.is_none());
        let mut e = RowEdit::from_spec(&s);
        e.lo = "abc".into(); // invalid: band left as it was
        e.notch = "".into(); // cleared
        e.extra = "".into();
        e.y = "10, 1".into(); // lo >= hi: ignored
        e.apply(&mut s);
        assert_eq!(s.extra["band"], json!([200.0, null]));
        assert!(s.extra.get("notch").is_none() && s.extra.get("order").is_none() && s.ylim.is_none());
    }

    #[test]
    fn preset_json_shape() {
        let p = Preset { time_base: Some(30.0), channels: vec![spec(), Spec::new("CH1", "slow")], overview: None, layout: None, labels: vec![] };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["channels"][0]["ref"], "CH6");
        assert_eq!(v["channels"][0]["band"], json!([100.0, 3000.0]));
        assert!(v["channels"][1].get("ref").is_none());
        let back: Preset = serde_json::from_value(v).unwrap();
        assert_eq!(back.channels, p.channels);
    }
}
