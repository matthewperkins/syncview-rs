//! Channel specs and presets (same JSON as the Python syncview).

use crate::filters::MODES;
use crate::oe::Recording;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const MAX_SHOWN: usize = 16; // rows shown by the recording-derived preset; the rest start hidden

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Spec {
    pub ch: String,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default = "yes")]
    pub show: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ylim: Option<[f64; 2]>,
    /// How far the trace may draw beyond its own row (display only).
    #[serde(default, skip_serializing_if = "Clip::is_row")]
    pub clip: Clip,
    /// Filter overrides (band, order, notch, plot_fs, …) and anything else in the JSON.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn yes() -> bool {
    true
}

/// Where a row's trace is cut off: at its row, at the edges of the chart (all rows), or nowhere
/// (it may also draw over the video and the overview strip). Its scale is always its own row's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Clip {
    #[default]
    Row,
    Chart,
    #[serde(rename = "none")]
    Unclipped,
}

impl Clip {
    pub const ALL: [Clip; 3] = [Clip::Row, Clip::Chart, Clip::Unclipped];

    fn is_row(&self) -> bool {
        *self == Clip::Row
    }

    pub fn name(self) -> &'static str {
        match self {
            Clip::Row => "row",
            Clip::Chart => "chart",
            Clip::Unclipped => "none",
        }
    }
}

impl Spec {
    pub fn new(ch: &str, mode: &str) -> Self {
        Spec { ch: ch.into(), reference: None, mode: mode.into(), label: None, show: true, color: None, ylim: None, clip: Clip::Row, extra: Map::new() }
    }

    pub fn title(&self) -> String {
        self.label.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| self.ch.clone())
    }

    /// The spec without its display-only fields (what the processed trace depends on).
    pub fn processing(&self) -> Spec {
        Spec { label: None, show: true, color: None, ylim: None, clip: Clip::Row, ..self.clone() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Preset {
    #[serde(default)]
    pub time_base: Option<f64>,
    #[serde(default)]
    pub channels: Vec<Spec>,
    #[serde(default)]
    pub overview: Option<Spec>,
    /// Video placement and trace styling (absent = stacked, no outline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<Layout>,
    /// The annotation labels this project uses (bite, chewing bout, contraction, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<Label>,
}

/// An annotation label: its name, the key that marks it, whether it marks a moment or a stretch,
/// and its colour ("#rrggbb"; absent: picked from a palette).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Label {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    #[serde(default)]
    pub kind: LabelKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LabelKind {
    Point,
    #[default]
    Interval,
}

impl Label {
    /// For the annotations file header: "bite (interval, key B)".
    pub fn describe(&self) -> String {
        let kind = if self.kind == LabelKind::Point { "point" } else { "interval" };
        if self.key.is_empty() { format!("{} ({kind})", self.name) } else { format!("{} ({kind}, key {})", self.name, self.key) }
    }
}

/// Where floating videos 1-4 start: the four quarters of the viewing area, top right first.
pub const VIDEO_BOXES: [[f32; 3]; 4] = [[0.5, 0.0, 0.5], [0.0, 0.0, 0.5], [0.0, 0.5, 0.5], [0.5, 0.5, 0.5]];

fn one_or_many_boxes<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<[f32; 3]>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Boxes {
        One([f32; 3]),
        Many(Vec<[f32; 3]>),
    }
    Ok(match Boxes::deserialize(d)? {
        Boxes::One(b) => vec![b],
        Boxes::Many(v) => v,
    })
}

/// How the video and traces are arranged and drawn (display only).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    /// The video is a movable box in the viewing area instead of sitting above the chart
    /// ("overlay" in v0.2.3 presets).
    #[serde(alias = "overlay")]
    pub float_video: bool,
    /// Each floating video's left and top edges (fractions of the viewing area) and its width
    /// (fraction of the area's width), in the order the videos are listed; its height follows
    /// the video's aspect ratio. A video without an entry starts at VIDEO_BOXES. (v0.2.3 and
    /// v0.2.4 presets hold one box, as "video".)
    #[serde(alias = "video", deserialize_with = "one_or_many_boxes")]
    pub videos: Vec<[f32; 3]>,
    /// The chart (rows, axes and labels) is a movable box instead of filling the area.
    pub float_chart: bool,
    /// The floating chart's left, top, width and height, as fractions of the viewing area.
    pub chart: [f32; 4],
    /// Drags pan time instead of moving and resizing the floating boxes.
    pub locked: bool,
    /// How much the video is darkened where the chart covers it, 0 to 0.9.
    pub dim: f32,
    /// A dark line under each trace, extending outline_px beyond it on each side.
    pub outline: bool,
    pub outline_px: f32,
}

impl Default for Layout {
    fn default() -> Self {
        Layout {
            float_video: false,
            videos: vec![],
            float_chart: false,
            chart: [0.0, 0.45, 0.6, 0.55],
            locked: false,
            dim: 0.4,
            outline: false,
            outline_px: 1.5,
        }
    }
}

/// Default preset: every electrode channel (Open Ephys type 0 / uV; ADC and AUX skipped), labelled by
/// channel name, drawn as EMG (hilo); the first MAX_SHOWN rows visible.
pub fn from_recording(rec: &Recording) -> Preset {
    let mut chans: Vec<&String> = rec.ch_names.iter().zip(&rec.ch_types).filter(|(_, t)| **t == Some(0)).map(|(n, _)| n).collect();
    if chans.is_empty() {
        chans = rec.ch_names.iter().zip(&rec.units).filter(|(_, u)| *u == "uV").map(|(n, _)| n).collect();
    }
    if chans.is_empty() {
        chans = rec.ch_names.iter().collect();
    }
    let channels = chans
        .iter()
        .enumerate()
        .map(|(i, n)| Spec { label: Some((*n).clone()), show: i < MAX_SHOWN, ..Spec::new(n, "hilo") })
        .collect();
    Preset { time_base: Some(10.0), channels, overview: None, layout: None, labels: vec![] }
}

/// Drop rows (and the overview) that refer to channels or modes this recording doesn't have.
pub fn check(mut p: Preset, rec: &Recording) -> (Preset, Vec<String>) {
    let bad = |s: &Spec| -> Option<String> {
        let missing: Vec<&str> = [Some(&s.ch), s.reference.as_ref()]
            .into_iter()
            .flatten()
            .filter(|c| !c.is_empty() && rec.ch(c).is_none())
            .map(|c| c.as_str())
            .collect();
        if !missing.is_empty() {
            return Some(format!("channel {} not in this recording", missing.join(", ")));
        }
        if !MODES.contains(&s.mode.as_str()) {
            return Some(format!("unknown mode {:?}", s.mode));
        }
        None
    };
    let mut problems = vec![];
    p.channels.retain(|s| match bad(s) {
        Some(why) => {
            problems.push(format!("row {:?} skipped: {why}", s.title()));
            false
        }
        None => true,
    });
    if let Some(o) = &p.overview {
        if let Some(why) = bad(o) {
            problems.push(format!("overview {:?} skipped: {why}", o.title()));
            p.overview = None;
        }
    }
    (p, problems)
}

#[test]
fn clip_round_trips_and_stays_out_of_default_presets() {
    let s: Spec = serde_json::from_str(r#"{"ch": "CH1", "mode": "hilo", "clip": "none"}"#).unwrap();
    assert_eq!(s.clip, Clip::Unclipped);
    assert!(s.extra.is_empty());
    assert_eq!(serde_json::to_value(&s).unwrap()["clip"], "none");
    // display only: the processed trace (and its cache entry) doesn't depend on it
    assert_eq!(s.processing(), Spec::new("CH1", "hilo").processing());
    let row = Spec::new("CH1", "hilo");
    assert!(serde_json::to_value(&row).unwrap().get("clip").is_none());
}

#[test]
fn layout_is_optional_and_fills_in_defaults() {
    let p: Preset = serde_json::from_str(r#"{"channels": []}"#).unwrap();
    assert!(p.layout.is_none());
    assert!(serde_json::to_value(&p).unwrap().get("layout").is_none());
    // v0.2.3 wrote "overlay" for what is now float_video
    let p: Preset = serde_json::from_str(r#"{"channels": [], "layout": {"overlay": true, "dim": 0.6}}"#).unwrap();
    let l = p.layout.unwrap();
    assert!(l.float_video && !l.float_chart && l.dim == 0.6);
    assert!(l.videos.is_empty());
    // one floating video box (v0.2.3 / v0.2.4), or one per video
    let p: Preset = serde_json::from_str(r#"{"channels": [], "layout": {"video": [0.1, 0.2, 0.3]}}"#).unwrap();
    assert_eq!(p.layout.unwrap().videos, vec![[0.1, 0.2, 0.3]]);
    let p: Preset = serde_json::from_str(r#"{"channels": [], "layout": {"videos": [[0.1, 0.2, 0.3], [0.5, 0.5, 0.5]]}}"#).unwrap();
    assert_eq!(p.layout.unwrap().videos.len(), 2);
    assert!(!l.outline && l.outline_px == Layout::default().outline_px);
}
