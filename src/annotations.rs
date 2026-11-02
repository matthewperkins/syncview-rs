//! Annotated events (bites, chewing bouts, contractions, …): kept in memory while viewing and
//! saved next to the recording as `syncview_annotations.csv`, one row per event, with
//! `syncview_overlaps.csv` (every pair of events that overlap) regenerated beside it on each save.
//!
//! Times are seconds from the recording's first sample, and sample indexes count from 0 there,
//! as spike sorters (Kilosort) count, so events line up with sorted spikes without conversion.
//! Saving never loses data: the file is written to a temporary name and renamed, a file that
//! failed to load is never overwritten, and columns this version doesn't know (written by a
//! newer one) are kept.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const FILE_NAME: &str = "syncview_annotations.csv";
pub const OVERLAPS_NAME: &str = "syncview_overlaps.csv";

/// The columns this version reads and writes, in file order.
const COLUMNS: [&str; 14] = [
    "id",
    "label",
    "start_s",
    "end_s",
    "start_sample",
    "end_sample",
    "frame_start",
    "frame_end",
    "video",
    "note",
    "annotator",
    "view",
    "marked_at",
    "kind",
];

/// One marked event. A point has start == end.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// Stable for the event's life ("e00042"), so edits and merges can be traced.
    pub id: String,
    pub label: String,
    pub start_s: f64,
    pub end_s: f64,
    /// Sample index from the recording's first sample (0-based).
    pub start_sample: i64,
    pub end_sample: i64,
    /// Reference-camera frames, when marked against a video.
    pub frame_start: Option<i64>,
    pub frame_end: Option<i64>,
    /// The reference camera's file name.
    pub video: String,
    pub note: String,
    pub annotator: String,
    /// "video+emg", or "video" when scored with the traces hidden (audit mode).
    pub view: String,
    /// When it was marked or last edited (UTC, ISO 8601).
    pub marked_at: String,
    /// "point" or "interval".
    pub kind: String,
    /// Columns this version doesn't know, kept as read.
    pub extra: BTreeMap<String, String>,
}

impl Event {
    pub fn is_point(&self) -> bool {
        self.kind == "point"
    }
}

/// Where a recording's annotations live: in its folder (an NWB file: beside it, named after it).
pub fn file_for(rec_dir: &Path) -> PathBuf {
    sibling(rec_dir, FILE_NAME)
}

fn sibling(rec_dir: &Path, name: &str) -> PathBuf {
    if rec_dir.is_dir() {
        rec_dir.join(name)
    } else {
        let stem = rec_dir.file_stem().unwrap_or_default().to_string_lossy();
        rec_dir.with_file_name(format!("{stem}_{name}"))
    }
}

/// What the file header records about the recording (comment lines, skipped by readers).
#[derive(Clone, Debug, Default)]
pub struct Header {
    pub recording: String,
    pub sample_rate: f64,
    /// Open Ephys's own sample number of the first sample (what `*_sample` 0 corresponds to).
    pub first_sample_number: i64,
    /// One line per label: "bite (interval, key B)".
    pub labels: Vec<String>,
}

/// A recording's events, and where they are saved.
pub struct Annotations {
    pub path: PathBuf,
    pub events: Vec<Event>,
    pub header: Header,
    /// Unknown columns seen in the file, in the order first seen (written back after ours).
    extra_columns: Vec<String>,
    next_id: u64,
    /// Set by a change; the store is saved once it is a second old (see `save_if_due`).
    changed: Option<Instant>,
    /// Why the existing file couldn't be read: then nothing is ever written over it.
    pub load_error: Option<String>,
}

// (the editing methods are called once marking lands: annotation plan, step 2)
#[allow(dead_code)]
impl Annotations {
    /// The recording's events (none yet if there is no file).
    pub fn load(path: PathBuf, header: Header) -> Annotations {
        let mut a = Annotations { path, events: vec![], header, extra_columns: vec![], next_id: 1, changed: None, load_error: None };
        if a.path.exists() {
            match read(&a.path) {
                Ok((events, extra)) => {
                    a.next_id = events.iter().filter_map(|e| e.id.strip_prefix('e')?.parse::<u64>().ok()).max().unwrap_or(0) + 1;
                    a.events = events;
                    a.extra_columns = extra;
                }
                Err(e) => a.load_error = Some(format!("{e:#}")),
            }
        }
        a
    }

    /// Add an event (its id is assigned here); returns the id.
    pub fn add(&mut self, mut e: Event) -> String {
        e.id = format!("e{:05}", self.next_id);
        self.next_id += 1;
        let id = e.id.clone();
        self.events.push(e);
        self.sort();
        self.touch();
        id
    }

    /// Put back an event that was removed (undo / redo), keeping its id.
    pub fn restore(&mut self, e: Event) {
        if let Some(n) = e.id.strip_prefix('e').and_then(|n| n.parse::<u64>().ok()) {
            self.next_id = self.next_id.max(n + 1);
        }
        self.events.push(e);
        self.sort();
        self.touch();
    }

    /// Replace the event with the same id (an edit); false if there is none.
    pub fn replace(&mut self, e: Event) -> bool {
        let Some(slot) = self.events.iter_mut().find(|x| x.id == e.id) else { return false };
        *slot = e;
        self.sort();
        self.touch();
        true
    }

    pub fn get(&self, id: &str) -> Option<&Event> {
        self.events.iter().find(|e| e.id == id)
    }

    /// Remove the event with this id; returns it.
    pub fn remove(&mut self, id: &str) -> Option<Event> {
        let i = self.events.iter().position(|e| e.id == id)?;
        self.touch();
        Some(self.events.remove(i))
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Event> {
        self.touch();
        self.events.iter_mut().find(|e| e.id == id)
    }

    fn sort(&mut self) {
        self.events.sort_by(|a, b| a.start_s.total_cmp(&b.start_s).then(a.id.cmp(&b.id)));
    }

    fn touch(&mut self) {
        self.changed = Some(Instant::now());
    }

    /// Unsaved changes.
    pub fn dirty(&self) -> bool {
        self.changed.is_some()
    }

    /// Save if the last change is at least `after` old (call every frame; Duration::ZERO saves now).
    pub fn save_if_due(&mut self, after: Duration) -> Result<bool> {
        match self.changed {
            Some(t) if t.elapsed() >= after => {
                self.save()?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Write the annotations (atomically) and regenerate the overlaps file.
    pub fn save(&mut self) -> Result<()> {
        if let Some(e) = &self.load_error {
            bail!("{} couldn't be read ({e}), so it isn't overwritten; fix or move it first", self.path.display());
        }
        self.sort();
        write_atomic(&self.path, &self.to_csv()?)?;
        // beside it, named alike ("<stem>_syncview_overlaps.csv" next to an NWB file)
        let name = self.path.file_name().unwrap_or_default().to_string_lossy().replace(FILE_NAME, OVERLAPS_NAME);
        write_atomic(&self.path.with_file_name(name), &overlaps_csv(&self.events, &self.path)?)?;
        self.changed = None;
        Ok(())
    }

    fn to_csv(&self) -> Result<Vec<u8>> {
        let h = &self.header;
        let mut out = format!(
            "# syncview {} annotations: one row per event; a point has start = end.\n\
             # start_s / end_s: seconds from the recording's first sample; *_sample: sample index from 0 there\n\
             # (as Kilosort counts); frame_*: reference-camera frames.\n\
             # recording: {}\n\
             # sample_rate: {}\n\
             # first_sample_number: {} (Open Ephys's own number of sample index 0)\n",
            env!("CARGO_PKG_VERSION"),
            h.recording,
            h.sample_rate,
            h.first_sample_number
        )
        .into_bytes();
        for l in &h.labels {
            out.extend(format!("# label: {l}\n").as_bytes());
        }
        let mut w = csv::Writer::from_writer(out);
        let mut header: Vec<&str> = COLUMNS.to_vec();
        header.extend(self.extra_columns.iter().map(String::as_str));
        w.write_record(&header)?;
        for e in &self.events {
            let opt = |v: Option<i64>| v.map(|x| x.to_string()).unwrap_or_default();
            let mut row = vec![
                e.id.clone(),
                e.label.clone(),
                format!("{:.6}", e.start_s),
                format!("{:.6}", e.end_s),
                e.start_sample.to_string(),
                e.end_sample.to_string(),
                opt(e.frame_start),
                opt(e.frame_end),
                e.video.clone(),
                e.note.clone(),
                e.annotator.clone(),
                e.view.clone(),
                e.marked_at.clone(),
                e.kind.clone(),
            ];
            row.extend(self.extra_columns.iter().map(|c| e.extra.get(c).cloned().unwrap_or_default()));
            w.write_record(&row)?;
        }
        w.into_inner().map_err(|e| anyhow::anyhow!("{e}"))
    }
}

/// Events and unknown column names from a file.
fn read(path: &Path) -> Result<(Vec<Event>, Vec<String>)> {
    let mut r = csv::ReaderBuilder::new().comment(Some(b'#')).flexible(true).from_path(path).with_context(|| format!("opening {}", path.display()))?;
    let header: Vec<String> = r.headers()?.iter().map(str::to_string).collect();
    for need in ["label", "start_s", "end_s"] {
        if !header.iter().any(|h| h == need) {
            bail!("no {need:?} column");
        }
    }
    let extra: Vec<String> = header.iter().filter(|h| !COLUMNS.contains(&h.as_str())).cloned().collect();
    let mut events = vec![];
    for (n, rec) in r.records().enumerate() {
        let rec = rec.with_context(|| format!("row {}", n + 1))?;
        let mut f: BTreeMap<&str, String> = BTreeMap::new();
        for (h, v) in header.iter().zip(rec.iter()) {
            f.insert(h.as_str(), v.to_string());
        }
        let get = |k: &str| f.get(k).cloned().unwrap_or_default();
        let num = |k: &str| -> Result<f64> { get(k).trim().parse::<f64>().with_context(|| format!("row {}: {k} {:?} isn't a number", n + 1, get(k))) };
        let int = |k: &str| get(k).trim().parse::<i64>().ok();
        let (start_s, end_s) = (num("start_s")?, num("end_s")?);
        events.push(Event {
            id: get("id"),
            label: get("label"),
            start_s,
            end_s,
            start_sample: int("start_sample").unwrap_or(0),
            end_sample: int("end_sample").unwrap_or(0),
            frame_start: int("frame_start"),
            frame_end: int("frame_end"),
            video: get("video"),
            note: get("note"),
            annotator: get("annotator"),
            view: get("view"),
            marked_at: get("marked_at"),
            kind: if f.contains_key("kind") { get("kind") } else if start_s == end_s { "point".into() } else { "interval".into() },
            extra: extra.iter().map(|c| (c.clone(), get(c))).collect(),
        });
    }
    Ok((events, extra))
}

/// Every overlapping pair (two events sharing some time; a point inside an interval, or two
/// points at the same instant, count): same-label pairs first, then by time.
pub fn overlaps(events: &[Event]) -> Vec<(usize, usize, f64, f64)> {
    let mut idx: Vec<usize> = (0..events.len()).collect();
    idx.sort_by(|a, b| events[*a].start_s.total_cmp(&events[*b].start_s));
    let mut out = vec![];
    for (i, &a) in idx.iter().enumerate() {
        let ea = &events[a];
        for &b in &idx[i + 1..] {
            let eb = &events[b];
            if eb.start_s > ea.end_s {
                break;
            }
            let (s, e) = (ea.start_s.max(eb.start_s), ea.end_s.min(eb.end_s));
            // intervals that only touch end-to-start don't overlap; a point on an edge does
            let touching = e == s && !ea.is_point() && !eb.is_point();
            if e >= s && !touching {
                out.push((a, b, s, e));
            }
        }
    }
    out.sort_by(|x, y| {
        let same = |p: &(usize, usize, f64, f64)| events[p.0].label == events[p.1].label;
        same(y).cmp(&same(x)).then(x.2.total_cmp(&y.2))
    });
    out
}

/// How a label's video-only (audit) scoring compares with its normal scoring.
#[derive(Clone, Debug, PartialEq)]
pub struct Agreement {
    pub label: String,
    /// The stretch the audit pass covers (first audit event's start to the last one's end);
    /// normal events are compared only there.
    pub span: (f64, f64),
    pub audit: usize,
    pub normal: usize,
    pub matched: usize,
    /// Mean |difference| of matched events' onsets and offsets, in seconds.
    pub onset_diff: Option<f64>,
    pub offset_diff: Option<f64>,
}

/// Two passes match an event when they overlap, or for points (or very short marks) when their
/// onsets are within this many seconds.
pub const MATCH_TOLERANCE_S: f64 = 0.1;

/// For each label scored in audit mode (view "video"): the audit events against the normal ones
/// in the same stretch, paired greedily in time order by the closest onset.
pub fn agreement(events: &[Event]) -> Vec<Agreement> {
    let mut labels: Vec<&str> = events.iter().filter(|e| e.view == "video").map(|e| e.label.as_str()).collect();
    labels.sort();
    labels.dedup();
    labels
        .into_iter()
        .map(|label| {
            let audit: Vec<&Event> = events.iter().filter(|e| e.label == label && e.view == "video").collect();
            let span = (audit.iter().map(|e| e.start_s).fold(f64::INFINITY, f64::min), audit.iter().map(|e| e.end_s).fold(f64::NEG_INFINITY, f64::max));
            let normal: Vec<&Event> =
                events.iter().filter(|e| e.label == label && e.view != "video" && e.end_s >= span.0 && e.start_s <= span.1).collect();
            let mut used = vec![false; normal.len()];
            let (mut on, mut off, mut matched) = (0.0, 0.0, 0usize);
            for a in &audit {
                let best = normal
                    .iter()
                    .enumerate()
                    .filter(|(j, n)| {
                        !used[*j] && ((n.start_s < a.end_s && a.start_s < n.end_s) || (n.start_s - a.start_s).abs() <= MATCH_TOLERANCE_S)
                    })
                    .min_by(|(_, x), (_, y)| (x.start_s - a.start_s).abs().total_cmp(&(y.start_s - a.start_s).abs()));
                if let Some((j, n)) = best {
                    used[j] = true;
                    matched += 1;
                    on += (n.start_s - a.start_s).abs();
                    off += (n.end_s - a.end_s).abs();
                }
            }
            let mean = |x: f64| (matched > 0).then(|| x / matched as f64);
            Agreement { label: label.to_string(), span, audit: audit.len(), normal: normal.len(), matched, onset_diff: mean(on), offset_diff: mean(off) }
        })
        .collect()
}

fn overlaps_csv(events: &[Event], from: &Path) -> Result<Vec<u8>> {
    let head = format!(
        "# syncview {}: every pair of overlapping events in {}, same-label pairs first.\n\
         # Regenerated on every save; edits here are lost. Safe to delete.\n",
        env!("CARGO_PKG_VERSION"),
        from.file_name().unwrap_or_default().to_string_lossy()
    );
    let mut w = csv::Writer::from_writer(head.into_bytes());
    w.write_record(["same_label", "id_a", "label_a", "annotator_a", "id_b", "label_b", "annotator_b", "start_s", "end_s", "overlap_s"])?;
    for (a, b, s, e) in overlaps(events) {
        let (ea, eb) = (&events[a], &events[b]);
        w.write_record([
            (ea.label == eb.label).to_string(),
            ea.id.clone(),
            ea.label.clone(),
            ea.annotator.clone(),
            eb.id.clone(),
            eb.label.clone(),
            eb.annotator.clone(),
            format!("{s:.6}"),
            format!("{e:.6}"),
            format!("{:.6}", e - s),
        ])?;
    }
    w.into_inner().map_err(|e| anyhow::anyhow!("{e}"))
}

/// Write via a temporary file and a rename, so a crash never leaves half a file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("csv.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

/// Now, as UTC ISO 8601 to the second ("2026-11-03T19:05:12Z").
#[allow(dead_code)]
pub fn now_utc() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Howard Hinnant's civil_from_days
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(label: &str, a: f64, b: f64) -> Event {
        Event {
            id: String::new(),
            label: label.into(),
            start_s: a,
            end_s: b,
            start_sample: (a * 10000.0).round() as i64,
            end_sample: (b * 10000.0).round() as i64,
            frame_start: Some((a * 30.0) as i64),
            frame_end: Some((b * 30.0) as i64),
            video: "face, close-up.mp4".into(),
            note: "incisal, \"pellet\"\nheld in both paws".into(),
            annotator: "MP".into(),
            view: "video+emg".into(),
            marked_at: now_utc(),
            kind: if a == b { "point".into() } else { "interval".into() },
            extra: BTreeMap::new(),
        }
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("syncview_ann_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn round_trip_keeps_every_field_and_unknown_columns() {
        let d = tmpdir("rt");
        let path = file_for(&d);
        let mut a = Annotations::load(path.clone(), Header { recording: "r".into(), sample_rate: 10000.0, labels: vec!["bite (interval, key B)".into()], ..Default::default() });
        assert!(a.events.is_empty() && a.load_error.is_none());
        a.add(ev("chewing bout", 10.0, 25.5));
        a.add(ev("bite", 12.25, 12.4));
        a.add(ev("pup delivered", 3.0, 3.0));
        a.save().unwrap();
        let b = Annotations::load(path.clone(), Header::default());
        assert_eq!(b.events, a.events);
        assert_eq!(b.events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["e00003", "e00001", "e00002"]); // sorted by time

        // a newer version added a column: it survives a load / edit / save here
        let text = std::fs::read_to_string(&path).unwrap();
        let text = text.replacen("kind\n", "kind,confidence\n", 1).replace("point\n", "point,0.9\n").replace("interval\n", "interval,0.5\n");
        std::fs::write(&path, text).unwrap();
        let mut c = Annotations::load(path.clone(), Header::default());
        assert_eq!(c.events[0].extra.get("confidence").map(String::as_str), Some("0.9"));
        let id = c.add(ev("grooming bout", 40.0, 41.0));
        assert_eq!(id, "e00004");
        c.save().unwrap();
        let d2 = Annotations::load(path.clone(), Header::default());
        assert_eq!(d2.events.len(), 4);
        assert_eq!(d2.events[0].extra.get("confidence").map(String::as_str), Some("0.9"));
        assert_eq!(d2.events[3].extra.get("confidence").map(String::as_str), Some(""));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_file_that_cannot_be_read_is_never_overwritten() {
        let d = tmpdir("bad");
        let path = file_for(&d);
        std::fs::write(&path, "something else entirely\n1,2\n").unwrap();
        let mut a = Annotations::load(path.clone(), Header::default());
        assert!(a.load_error.is_some());
        a.add(ev("bite", 1.0, 1.1));
        assert!(a.save().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "something else entirely\n1,2\n");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn overlaps_pairs_same_label_first() {
        let es = vec![
            ev("chewing bout", 10.0, 20.0),
            ev("gastric episode", 15.0, 60.0),
            ev("bite", 12.0, 12.2),
            ev("bite", 12.1, 12.3),  // overlaps the previous bite: a double mark?
            ev("bite", 20.0, 20.1),  // touches the chewing bout's end: an interval edge, not an overlap
            ev("pup delivered", 30.0, 30.0), // a point inside the gastric episode
        ];
        let o = overlaps(&es);
        let names: Vec<(String, String)> = o.iter().map(|(a, b, ..)| (es[*a].label.clone(), es[*b].label.clone())).collect();
        assert_eq!(names[0], ("bite".into(), "bite".into()));
        assert!(names.contains(&("chewing bout".into(), "gastric episode".into())));
        assert!(names.contains(&("gastric episode".into(), "pup delivered".into())));
        // chewing 10-20 overlaps bites at 12.0 and 12.1, gastric 15-60 overlaps the bite at 20.0;
        // the bite at 20.0-20.1 only touches the chewing bout's end
        let n_chew_bite = o.iter().filter(|(a, b, ..)| (es[*a].label == "chewing bout") ^ (es[*b].label == "chewing bout") && (es[*a].label == "bite" || es[*b].label == "bite")).count();
        assert_eq!(n_chew_bite, 2);
        assert_eq!(o.len(), 1 + 1 + 1 + 2 + 1);
    }

    #[test]
    fn agreement_pairs_audit_and_normal_passes() {
        let with_view = |label: &str, a: f64, b: f64, view: &str| Event { view: view.into(), ..ev(label, a, b) };
        let es = vec![
            // normal pass (video + EMG)
            with_view("bite", 10.00, 10.15, "video+emg"),
            with_view("bite", 20.00, 20.12, "video+emg"),
            with_view("bite", 30.00, 30.10, "video+emg"), // missed in the audit pass
            with_view("bite", 90.00, 90.10, "video+emg"), // outside the audited stretch: not compared
            // audit pass (video only), 10-35 s
            with_view("bite", 10.03, 10.18, "video"),
            with_view("bite", 20.05, 20.20, "video"),
            with_view("bite", 34.90, 35.00, "video"), // not in the normal pass
            with_view("chewing bout", 5.0, 9.0, "video+emg"),
        ];
        let a = agreement(&es);
        assert_eq!(a.len(), 1);
        let b = &a[0];
        assert_eq!((b.label.as_str(), b.audit, b.normal, b.matched), ("bite", 3, 3, 2));
        assert_eq!(b.span, (10.03, 35.0));
        assert!((b.onset_diff.unwrap() - 0.04).abs() < 1e-9, "{:?}", b.onset_diff);
        assert!((b.offset_diff.unwrap() - 0.055).abs() < 1e-9, "{:?}", b.offset_diff);
    }

    #[test]
    fn utc_time_looks_right() {
        let t = now_utc();
        assert!(t.len() == 20 && t.ends_with('Z') && t.starts_with("20"), "{t}");
    }
}
