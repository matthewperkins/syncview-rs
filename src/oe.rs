//! The recording as the viewer sees it: one continuous stream of an Open Ephys recording, its
//! camera TTL lines, and video-frame <-> sample synchronisation. Reading is done by the
//! `openephys` crate; this keeps the handful of things the viewer needs in one place.
//!
//! A recording can also be a portable session (portable.rs): the same facts read from a small
//! folder, for viewing and annotating its cached traces without the recording itself.

use anyhow::Result;
use openephys::{Continuous, TtlSource};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use openephys::sync::median;

pub struct Recording {
    /// The recording's folder (for a portable session: where it was when the session was
    /// exported; cache keys are made from it).
    pub rec_dir: PathBuf,
    /// Where its annotations live: the recording's folder, or the portable session's.
    pub home: PathBuf,
    pub fs: f64,
    pub ch_names: Vec<String>,
    pub units: Vec<String>,
    pub ch_types: Vec<Option<i64>>, // Open Ephys: 0 ephys, 1 aux, 2 adc
    src: Source,
}

enum Source {
    OpenEphys { rec: Box<openephys::Recording>, stream: usize },
    Portable(crate::portable::Portable),
}

impl Recording {
    /// `stream` None: `acquisition_board` if there is one (GUI 1.0), else the first stream (GUI 0.6
    /// calls it e.g. `Rhythm Data`).
    pub fn open(rec_dir: &Path, stream: Option<&str>) -> Result<Self> {
        if crate::portable::is_session(rec_dir) {
            let (m, p) = crate::portable::load(rec_dir)?;
            let r = &m.recording;
            return Ok(Self {
                rec_dir: PathBuf::from(&r.path),
                home: rec_dir.to_path_buf(),
                fs: r.sample_rate,
                ch_names: r.channels.iter().map(|c| c.name.clone()).collect(),
                units: r.channels.iter().map(|c| c.unit.clone()).collect(),
                ch_types: r.channels.iter().map(|c| c.type_code).collect(),
                src: Source::Portable(p),
            });
        }
        let rec = openephys::Recording::open(rec_dir)?;
        let c = match stream {
            Some(s) => rec.stream(s)?,
            None => rec.stream("acquisition_board").or_else(|_| rec.main_stream())?,
        };
        let stream = c.name().to_string();
        let index = rec.continuous().iter().position(|x| std::ptr::eq(x, c)).unwrap();
        if !c.is_contiguous() {
            eprintln!("syncview: warning: continuous sample numbers are not contiguous (dropped samples?) - sync may be off.");
        }
        let s = Self {
            rec_dir: rec.dir().to_path_buf(),
            home: rec.dir().to_path_buf(),
            fs: c.sample_rate(),
            ch_names: c.channels().iter().map(|ch| ch.channel_name.clone()).collect(),
            units: c.channels().iter().map(|ch| ch.units.clone()).collect(),
            ch_types: c.channels().iter().map(|ch| ch.type_code).collect(),
            src: Source::OpenEphys { rec: Box::new(rec), stream: index },
        };
        if s.oe().ttl_for(s.cont()).next().is_none() {
            eprintln!("syncview: warning: no TTL events for stream {stream:?} - video sync is impossible.");
        }
        Ok(s)
    }

    /// A portable session (no raw data: nothing can be filtered).
    pub fn is_portable(&self) -> bool {
        matches!(self.src, Source::Portable(_))
    }

    /// The Open Ephys recording (not for a portable session: callers check is_portable first).
    fn oe(&self) -> &openephys::Recording {
        match &self.src {
            Source::OpenEphys { rec, .. } => rec,
            Source::Portable(_) => panic!("a portable session has no raw data"),
        }
    }

    fn cont(&self) -> &Continuous {
        match &self.src {
            Source::OpenEphys { rec, stream } => &rec.continuous()[*stream],
            Source::Portable(_) => panic!("a portable session has no raw data"),
        }
    }

    /// The stream's TTL source with events on `line` (the first source if none has).
    fn ttl(&self, line: i64) -> Option<&TtlSource> {
        let line = u16::try_from(line).ok();
        let mut sources = self.oe().ttl_for(self.cont()).peekable();
        let first = sources.peek().copied();
        sources.find(|t| line.is_some_and(|l| t.lines().contains_key(&l))).or(first)
    }

    /// Row of `continuous.dat` at a sample number (extrapolated outside the recording).
    fn row_of(&self, sample_number: i64) -> i64 {
        let c = self.cont();
        let first = c.first_sample_number().unwrap_or(0);
        if c.is_contiguous() {
            return sample_number - first;
        }
        match c.index_of(sample_number) {
            Ok(i) => i as i64,
            Err(0) => sample_number - first,
            Err(i) if i == c.n_samples() => i as i64 - 1 + (sample_number - c.sample_numbers()[i - 1]),
            Err(i) => i as i64,
        }
    }

    pub fn n_samples(&self) -> usize {
        match &self.src {
            Source::Portable(p) => p.n_samples,
            _ => self.cont().n_samples(),
        }
    }

    pub fn duration(&self) -> f64 {
        match &self.src {
            Source::Portable(p) => p.n_samples as f64 / self.fs,
            _ => self.cont().duration(),
        }
    }

    /// Open Ephys's own sample number of row 0 (0 when the recording doesn't say).
    pub fn first_sample_number(&self) -> i64 {
        match &self.src {
            Source::Portable(p) => p.first_sample_number,
            _ => self.cont().first_sample_number().unwrap_or(0),
        }
    }

    /// No gaps in the sample numbers (dropped samples).
    pub fn is_contiguous(&self) -> bool {
        match &self.src {
            Source::Portable(p) => p.contiguous,
            _ => self.cont().is_contiguous(),
        }
    }

    pub fn ch(&self, name: &str) -> Option<usize> {
        match &self.src {
            Source::Portable(_) => self.ch_names.iter().position(|n| n == name),
            _ => self.cont().channel_index(name),
        }
    }

    /// Rows of the rising edges on `line`.
    pub fn rising_edges(&self, line: i64) -> Vec<i64> {
        if let Source::Portable(p) = &self.src {
            return u16::try_from(line).ok().and_then(|l| p.ttl.get(&l).cloned()).unwrap_or_default();
        }
        match (self.ttl(line), u16::try_from(line)) {
            (Some(t), Ok(l)) => t.rising(l).into_iter().map(|sn| self.row_of(sn)).collect(),
            _ => vec![],
        }
    }

    /// Rows of the rising edges on every TTL line of the stream (for a portable session).
    pub fn ttl_rising_rows(&self) -> BTreeMap<u16, Vec<i64>> {
        if let Source::Portable(p) = &self.src {
            return p.ttl.clone();
        }
        let mut lines: Vec<u16> = self.oe().ttl_for(self.cont()).flat_map(|t| t.lines().into_keys()).collect();
        lines.sort_unstable();
        lines.dedup();
        lines.into_iter().map(|l| (l, self.rising_edges(l as i64))).collect()
    }

    /// Rows [i0, i1) clipped to the recording: (first row, number of rows).
    pub fn clip(&self, i0: i64, i1: i64) -> (usize, usize) {
        let n = self.n_samples();
        let a = (i0.max(0) as usize).min(n);
        let b = (i1.max(0) as usize).min(n);
        (a, b.saturating_sub(a))
    }

    /// Stored values of several channels for rows [first, first + n), one column per channel, in
    /// one pass over the rows (reading channels one at a time reads every row once per channel).
    pub fn raw_columns(&self, chans: &[usize], first: usize, n: usize) -> Vec<Vec<i16>> {
        self.raw_columns_by(chans, first, n, true)
    }

    /// (testing) Time reading `chans` over the whole recording in chunks of `chunk` rows, as a
    /// whole-session build does, through the memory map and with ordinary reads, twice each.
    pub fn bench_read(&self, chans: &[usize], chunk: usize) {
        let n = self.n_samples();
        let gb = (n * self.cont().n_channels() * 2) as f64 / 1e9;
        println!("reading {} of {} channels, {:.2} GB of continuous.dat, in {} chunks, on {} threads", chans.len(), self.cont().n_channels(), gb, n.div_ceil(chunk), rayon::current_num_threads());
        for (name, plain) in [("memory map", false), ("ordinary reads", true), ("memory map", false), ("ordinary reads", true)] {
            let t = std::time::Instant::now();
            let mut c0 = 0;
            while c0 < n {
                let len = chunk.min(n - c0);
                std::hint::black_box(self.raw_columns_by(chans, c0, len, plain));
                c0 += len;
            }
            let dt = t.elapsed().as_secs_f64();
            println!("  {name:<15} {dt:5.2} s  ({:.1} GB/s)", gb / dt);
        }
    }

    fn raw_columns_by(&self, chans: &[usize], first: usize, n: usize, plain_reads: bool) -> Vec<Vec<i16>> {
        use rayon::prelude::*;
        let c = self.cont();
        let nc = c.n_channels();
        let mut cols: Vec<Vec<i16>> = chans.iter().map(|_| vec![0; n]).collect();
        // split the rows between threads; each fills its own piece of every column, in place
        let part = n.div_ceil(rayon::current_num_threads().max(1)).max(65_536);
        let mut pieces: Vec<Vec<&mut [i16]>> = (0..n.div_ceil(part)).map(|_| vec![]).collect();
        for col in cols.iter_mut() {
            for (piece, out) in pieces.iter_mut().zip(col.chunks_mut(part)) {
                piece.push(out);
            }
        }
        // A binary recording's continuous.dat is read with ordinary reads, 256 kB at a time (a
        // buffer that stays in the CPU's cache): through the memory map every page costs a page
        // fault, which on macOS made reading take ~2x longer than all the filtering. On Linux the
        // two are about equally fast. NWB recordings, and any read error, use the map.
        let dat = std::fs::File::open(c.dir().join("continuous.dat"))
            .ok()
            .filter(|_| plain_reads)
            .filter(|f| f.metadata().is_ok_and(|m| m.len() == (c.n_samples() * nc * 2) as u64));
        pieces.into_par_iter().enumerate().for_each(|(p, mut outs)| {
            let a = first + p * part;
            let b = (a + part).min(first + n);
            // one column at a time, over at most 256 kB of rows (still in the CPU's cache from
            // the read): each column is written in order, with no per-element bounds checks
            let mut gather = |rows: &[i16], i0: usize| {
                let per = (256 << 10) / (2 * nc);
                for (s, sub) in rows.chunks(per * nc).enumerate() {
                    let at = i0 + s * per;
                    for (out, &ch) in outs.iter_mut().zip(chans) {
                        let out = &mut out[at..at + sub.len() / nc];
                        for (o, row) in out.iter_mut().zip(sub.chunks_exact(nc)) {
                            *o = row[ch];
                        }
                    }
                }
            };
            if let Some(f) = dat.as_ref().filter(|_| cfg!(target_endian = "little")) {
                let per = (256 << 10) / (2 * nc);
                let mut buf = vec![0i16; per * nc];
                let mut r = a;
                while r < b {
                    let k = per.min(b - r);
                    let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut buf[..k * nc]);
                    if read_at(f, bytes, (r * nc * 2) as u64).is_err() {
                        break;
                    }
                    gather(&buf[..k * nc], r - a);
                    r += k;
                }
                if r == b {
                    return;
                }
            }
            let mut i = 0;
            for blk in c.blocks(a..b) {
                gather(&blk, i);
                i += blk.len() / nc;
            }
        });
        cols
    }

    pub fn bit_volts(&self, ch: usize) -> f64 {
        self.cont().bit_volts(ch)
    }

    /// Physical-unit trace of `ch` (minus `reference`) for rows [first, first + n).
    pub fn trace(&self, ch: usize, reference: Option<usize>, first: usize, n: usize) -> Vec<f64> {
        match reference {
            None => self.cont().read(ch, first..first + n),
            Some(r) => self.cont().read_referenced(ch, r, first..first + n),
        }
    }
}

pub struct SyncInfo {
    pub frames: Vec<i64>, // row index of each frame's trigger
    pub issues: Vec<String>,
    pub extra: i64,
    pub period: f64,
}

/// Frame k <-> k-th rising edge on `line`; extra triggers are expected only at the END.
/// Err when alignment is impossible (no / too few triggers).
pub fn check_sync(rec: &Recording, n_frames: usize, line: i64, video_duration: Option<f64>) -> Result<SyncInfo, String> {
    let l = u16::try_from(line).map_err(|_| format!("bad trigger line {line}"))?;
    if let Source::Portable(p) = &rec.src {
        return check_sync_rows(p, rec.fs, n_frames, l, video_duration);
    }
    let Some(ttl) = rec.ttl(line) else {
        return Err(format!("No TTL events in this recording's stream, so the video can't be synced (trigger line {line})."));
    };
    let a = openephys::align_frames(rec.cont(), ttl, l, n_frames, video_duration).map_err(|e| {
        let e = e.to_string();
        if e.starts_with("No triggers") { format!("{e} Pick the camera trigger line with --trigger-line.") } else { e }
    })?;
    Ok(SyncInfo {
        frames: a.sample_numbers.iter().map(|sn| rec.row_of(*sn)).collect(),
        issues: a.issues,
        extra: a.extra as i64,
        period: a.period,
    })
}

/// check_sync for a portable session: openephys::align_frames' rules and messages, on the rising
/// edges' rows as the session stores them.
fn check_sync_rows(p: &crate::portable::Portable, fs: f64, n_frames: usize, line: u16, video_duration: Option<f64>) -> Result<SyncInfo, String> {
    let empty = vec![];
    let edges = p.ttl.get(&line).unwrap_or(&empty);
    let others: Vec<String> = p.ttl.iter().filter(|(l, e)| **l != line && !e.is_empty()).map(|(l, e)| format!("line {l}: {}", e.len())).collect();
    let others = if others.is_empty() { "none".to_string() } else { others.join(", ") };
    if edges.is_empty() {
        return Err(format!("No triggers on TTL line {line}. Rising edges on other lines: {others}. Pick the camera trigger line with --trigger-line."));
    }
    if n_frames == 0 {
        return Err("The video has no frames.".into());
    }
    if edges.len() < n_frames {
        let hint: Vec<String> =
            p.ttl.iter().filter(|(l, e)| **l != line && e.len() >= n_frames && e.len() <= n_frames + 2).map(|(l, _)| l.to_string()).collect();
        return Err(format!(
            "Fewer triggers ({}) on line {line} than video frames ({n_frames}): wrong recording/video pair or trigger line?{}",
            edges.len(),
            if hint.is_empty() { format!(" Other lines: {others}.") } else { format!(" Line(s) {} have a matching count.", hint.join(", ")) }
        ));
    }
    let extra = edges.len() - n_frames;
    let fr = &edges[..n_frames];
    let d: Vec<f64> = fr.windows(2).map(|w| (w[1] - w[0]) as f64).collect();
    let med = median(&d);
    let mut issues = vec![];
    if extra > 1 {
        issues.push(format!("{extra} extra triggers after the last frame (expected 0 or 1): frames may be missing from the video, which shifts the alignment."));
    }
    if extra >= 1 && ((edges[n_frames] - fr[n_frames - 1]) as f64) < 1.5 * med {
        issues.push("The first unused trigger follows the last frame by only one frame period, so it looks like a real frame rather than an end-of-recording trigger: the video may have lost a frame.".into());
    }
    let n_long = d.iter().filter(|v| **v > 1.5 * med).count();
    let n_short = d.iter().filter(|v| **v < 0.5 * med).count();
    if n_long > 0 {
        issues.push(format!("{n_long} trigger interval(s) > 1.5x the frame period inside the video (missed triggers or paused camera?): frames around them may be misaligned."));
    }
    if n_short > 0 {
        issues.push(format!("{n_short} trigger interval(s) < 0.5x the frame period (double or spurious triggers?): alignment after them is likely off."));
    }
    let span = (fr[n_frames - 1] - fr[0]) as f64 / fs;
    if let Some(vd) = video_duration
        && span > 0.0
        && (vd - span).abs() > (0.01 * span).max(2.0)
    {
        issues.push(format!("Video duration {vd:.1} s differs from the trigger span {span:.1} s by more than 1%: check that this is the right video and trigger line."));
    }
    if !p.contiguous {
        issues.push("The recording's sample numbers have gaps (dropped samples), so frames near them may be off.".into());
    }
    let outside = fr.iter().filter(|r| **r < 0 || **r >= p.n_samples as i64).count();
    if outside > 0 {
        issues.push(format!("{outside} frame trigger(s) fall outside the recorded samples."));
    }
    Ok(SyncInfo { frames: fr.to_vec(), issues, extra: extra as i64, period: med / fs })
}

/// Fill buf from the file at byte offset `at` (positional: threads may share the file).
fn read_at(f: &std::fs::File, buf: &mut [u8], at: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(f, buf, at)
    }
    #[cfg(windows)]
    {
        let (mut done, mut buf) = (0u64, buf);
        while !buf.is_empty() {
            match std::os::windows::fs::FileExt::seek_read(f, buf, at + done)? {
                0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                k => {
                    done += k as u64;
                    buf = &mut buf[k..];
                }
            }
        }
        Ok(())
    }
}
