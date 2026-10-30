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
}

/// How the video and traces are arranged and drawn (display only).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    /// The video floats behind the trace rows instead of sitting above them.
    pub overlay: bool,
    /// The floating video's left and top edges (fractions of the trace area) and its width
    /// (fraction of the area's width); its height follows the video's aspect ratio.
    pub video: [f32; 3],
    /// Drags pan time instead of moving the video.
    pub locked: bool,
    /// How much the floating video is darkened, 0 to 0.9.
    pub dim: f32,
    /// A dark line under each trace, extending outline_px beyond it on each side.
    pub outline: bool,
    pub outline_px: f32,
}

impl Default for Layout {
    fn default() -> Self {
        Layout { overlay: false, video: [0.5, 0.0, 0.5], locked: false, dim: 0.4, outline: false, outline_px: 1.5 }
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
    Preset { time_base: Some(10.0), channels, overview: None, layout: None }
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
    let p: Preset = serde_json::from_str(r#"{"channels": [], "layout": {"overlay": true, "dim": 0.6}}"#).unwrap();
    let l = p.layout.unwrap();
    assert!(l.overlay && l.dim == 0.6);
    assert_eq!(l.video, Layout::default().video);
    assert!(!l.outline && l.outline_px == Layout::default().outline_px);
}
