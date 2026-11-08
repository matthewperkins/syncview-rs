//! Portable sessions: everything the viewer needs from a recording, without the recording.
//!
//! Exported where the recording and its cache are (`export`), a session folder holds:
//!
//!   syncview_session.json     the manifest (written last: its presence marks a complete session)
//!   traces/<key>/             the cached traces, as in the cache (cache.rs), with the full-rate
//!                             ones' finest levels left out: meta.json's first_level says from
//!                             which level they start; above it they draw exactly as the full ones
//!   traces/video_index/       the videos' frame indexes, named <file name>.<size>.npy
//!   ttl/line_<n>.npy          rows of each TTL line's rising edges (camera triggers for sync)
//!   preset.json               the rows, their settings, the layout and the annotation labels
//!   syncview_annotations.csv  the annotations so far (if any); marking continues here
//!
//! Opened like a recording (oe::Recording::open on the folder), it can be viewed, synced to its
//! videos and annotated, with the filter settings it was exported with. Its annotations use the
//! recording's own times and sample indexes, so the file can go back next to the recording.

use crate::cache::{TraceCache, FACTOR};
use crate::oe::Recording;
use crate::preset::{Preset, Spec};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const MANIFEST: &str = "syncview_session.json";

/// Traces stored at or below this rate keep every level (they are small: slow-wave traces at
/// 1 kHz, envelopes, band power); faster ones (full-rate EMG) start at the chosen level.
const SMALL_RATE_HZ: f64 = 1000.0;

#[derive(Serialize, Deserialize)]
pub struct Manifest {
    pub syncview_session: u32,
    pub created_by: String,
    pub created: String,
    pub recording: RecordingInfo,
    /// Rising edges per TTL line: how many (the rows are in ttl/line_<n>.npy).
    pub ttl_lines: BTreeMap<u16, usize>,
    /// The finest pyramid level kept for full-rate traces (1: blocks of 8 samples, 2: 64, …).
    pub first_level: usize,
    pub traces: Vec<TraceInfo>,
    pub videos: Vec<VideoInfo>,
}

#[derive(Serialize, Deserialize)]
pub struct RecordingInfo {
    /// The recording's folder where it was exported (cache keys and annotation headers use it).
    pub path: String,
    pub sample_rate: f64,
    pub n_samples: usize,
    pub first_sample_number: i64,
    pub contiguous: bool,
    pub channels: Vec<ChannelInfo>,
}

#[derive(Serialize, Deserialize)]
pub struct ChannelInfo {
    pub name: String,
    pub unit: String,
    pub type_code: Option<i64>,
}

#[derive(Serialize, Deserialize)]
pub struct TraceInfo {
    pub key: String,
    pub label: String,
    pub first_level: usize,
}

#[derive(Serialize, Deserialize)]
pub struct VideoInfo {
    pub name: String,
    pub size: u64,
}

/// A portable session's recording facts (what oe::Recording answers from).
pub struct Portable {
    pub n_samples: usize,
    pub first_sample_number: i64,
    pub contiguous: bool,
    /// Rows of each line's rising edges.
    pub ttl: BTreeMap<u16, Vec<i64>>,
}

/// Whether `dir` is a portable session.
pub fn is_session(dir: &Path) -> bool {
    dir.join(MANIFEST).is_file()
}

/// Read a portable session: its manifest and recording facts.
pub fn load(dir: &Path) -> Result<(Manifest, Portable)> {
    let text = std::fs::read_to_string(dir.join(MANIFEST)).with_context(|| format!("reading {}", dir.join(MANIFEST).display()))?;
    let m: Manifest = serde_json::from_str(&text).with_context(|| format!("{} isn't a syncview session manifest", MANIFEST))?;
    if m.syncview_session != 1 {
        bail!("{} is from a newer syncview (format {}); update syncview to open it", dir.display(), m.syncview_session);
    }
    let mut ttl = BTreeMap::new();
    for line in m.ttl_lines.keys() {
        ttl.insert(*line, crate::npy::read_i64(&dir.join(format!("ttl/line_{line}.npy")))?);
    }
    let p = Portable {
        n_samples: m.recording.n_samples,
        first_sample_number: m.recording.first_sample_number,
        contiguous: m.recording.contiguous,
        ttl,
    };
    Ok((m, p))
}

/// The finest level a trace keeps in a session exported at `first_level` (0: all of it).
fn kept_from(dt: f64, first_level: usize) -> usize {
    if 1.0 / dt <= SMALL_RATE_HZ * 1.0001 { 0 } else { first_level }
}

/// A cached trace's folder, step (dt, s) and number of levels.
fn trace_files(cache: &TraceCache, spec: &Spec) -> Result<(PathBuf, f64, usize)> {
    let dir = cache.root.join(cache.key(spec));
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).with_context(|| format!("{} isn't cached yet", spec.title()))?)?;
    Ok((dir, meta["dt"].as_f64().context("meta.json: dt")?, meta["levels"].as_u64().unwrap_or(0) as usize))
}

/// The files a trace contributes to a session exported at `first_level`.
fn trace_payload(dir: &Path, dt: f64, levels: usize, first_level: usize) -> (usize, Vec<PathBuf>) {
    let from = kept_from(dt, first_level).min(levels);
    let mut files = vec![];
    if from == 0 {
        files.push(dir.join("level0.npy"));
    }
    for k in from.max(1)..=levels {
        files.push(dir.join(format!("min{k}.npy")));
        files.push(dir.join(format!("max{k}.npy")));
    }
    (from, files)
}

/// Bytes a session of these traces would take at `first_level` (traces not cached yet count 0).
pub fn estimate(cache: &TraceCache, specs: &[Spec], first_level: usize) -> u64 {
    specs
        .iter()
        .filter_map(|s| trace_files(cache, s).ok())
        .map(|(dir, dt, levels)| trace_payload(&dir, dt, levels, first_level).1.iter().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum::<u64>())
        .sum()
}

/// The narrowest view (seconds across `px` pixel columns) that full-rate traces still draw exactly
/// in a session exported at `first_level`.
pub fn exact_down_to(fs: f64, first_level: usize, px: f64) -> f64 {
    px * (FACTOR as f64).powi(first_level as i32) / fs
}

/// Write a portable session of `specs` (all cached) into the new folder `out`; with
/// `copy_videos`, the videos too (so one folder carries everything).
#[allow(clippy::too_many_arguments)]
pub fn export(
    rec: &Recording,
    cache: &TraceCache,
    specs: &[Spec],
    preset: &Preset,
    videos: &[PathBuf],
    index_dir: &Path,
    first_level: usize,
    copy_videos: bool,
    out: &Path,
    progress: &dyn Fn(f32),
) -> Result<()> {
    if out.exists() && std::fs::read_dir(out)?.next().is_some() {
        bail!("{} already exists and isn't empty", out.display());
    }
    std::fs::create_dir_all(out.join("traces").join("video_index"))?;
    std::fs::create_dir_all(out.join("ttl"))?;
    // what to copy, and how much
    let mut plan = vec![];
    let mut infos = vec![];
    for s in specs {
        let key = cache.key(s);
        if infos.iter().any(|i: &TraceInfo| i.key == key) {
            continue;
        }
        let (dir, dt, levels) = trace_files(cache, s)?;
        let (from, files) = trace_payload(&dir, dt, levels, first_level);
        infos.push(TraceInfo { key: key.clone(), label: s.title(), first_level: from });
        plan.push((key, dir, from, files));
    }
    let video_bytes: u64 = if copy_videos { videos.iter().filter_map(|v| v.metadata().ok()).map(|m| m.len()).sum() } else { 0 };
    let total: u64 = (plan.iter().flat_map(|p| p.3.iter()).filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum::<u64>() + video_bytes).max(1);
    let mut done = 0u64;
    for (key, dir, from, files) in &plan {
        let to = out.join("traces").join(key);
        std::fs::create_dir_all(&to)?;
        for f in files {
            done += copy_with_progress(f, &to.join(f.file_name().unwrap()), &mut |n| progress(((done + n) as f64 / total as f64) as f32))?;
        }
        let mut meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json"))?)?;
        meta["first_level"] = serde_json::json!(from);
        std::fs::write(to.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
    }
    // TTL lines (camera triggers)
    let mut ttl_lines = BTreeMap::new();
    for (line, rows) in rec.ttl_rising_rows() {
        crate::npy::write_i64(&out.join(format!("ttl/line_{line}.npy")), &rows)?;
        ttl_lines.insert(line, rows.len());
    }
    // the videos' frame indexes (building any that aren't yet), named so they are found wherever
    // the videos are copied
    let mut vids = vec![];
    for v in videos {
        let pts = crate::video::frame_pts(v, index_dir)?;
        let name = v.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let size = std::fs::metadata(v)?.len();
        crate::npy::write_i64(&out.join("traces").join("video_index").join(crate::video::portable_index_name(&name, size)), &pts)?;
        vids.push(VideoInfo { name, size });
    }
    if copy_videos {
        for v in videos {
            let name = v.file_name().context("a video without a file name")?;
            done += copy_with_progress(v, &out.join(name), &mut |n| progress(((done + n) as f64 / total as f64) as f32))?;
        }
    }
    std::fs::write(out.join("preset.json"), serde_json::to_string_pretty(preset)? + "\n")?;
    let ann = crate::annotations::file_for(&rec.home);
    if ann.is_file() {
        std::fs::copy(&ann, out.join(crate::annotations::FILE_NAME))?;
    }
    let m = Manifest {
        syncview_session: 1,
        created_by: format!("syncview {}", env!("CARGO_PKG_VERSION")),
        created: crate::annotations::now_utc(),
        recording: RecordingInfo {
            path: rec.rec_dir.display().to_string(),
            sample_rate: rec.fs,
            n_samples: rec.n_samples(),
            first_sample_number: rec.first_sample_number(),
            contiguous: rec.is_contiguous(),
            channels: (0..rec.ch_names.len())
                .map(|i| ChannelInfo { name: rec.ch_names[i].clone(), unit: rec.units[i].clone(), type_code: rec.ch_types[i] })
                .collect(),
        },
        ttl_lines,
        first_level,
        traces: infos,
        videos: vids,
    };
    std::fs::write(out.join(MANIFEST), serde_json::to_string_pretty(&m)? + "\n")?;
    progress(1.0);
    Ok(())
}

/// Copy a file in 8 MB pieces, reporting bytes copied; returns its size.
fn copy_with_progress(from: &Path, to: &Path, progress: &mut dyn FnMut(u64)) -> Result<u64> {
    use std::io::{Read, Write};
    let mut r = std::fs::File::open(from).with_context(|| format!("reading {}", from.display()))?;
    let mut w = std::fs::File::create(to).with_context(|| format!("writing {}", to.display()))?;
    let mut buf = vec![0u8; 8 << 20];
    let mut n = 0u64;
    loop {
        let k = r.read(&mut buf)?;
        if k == 0 {
            break;
        }
        w.write_all(&buf[..k])?;
        n += k as u64;
        progress(n);
    }
    Ok(n)
}
