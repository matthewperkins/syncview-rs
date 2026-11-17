//! What a row's channel is referenced to, from the spec's `ref` text:
//!
//!   (none)                        the channel as recorded
//!   CH7                           minus another channel (before filtering, as Python syncview)
//!   car:<op>:<variant>:CH1,CH2…   a common reference of those channels, computed AFTER filtering as
//!                                 SpikeInterface's common_reference does (each member filtered as
//!                                 the row is, then the per-sample median or mean). op: median |
//!                                 mean; variant: excl (the row's own channel left out of its
//!                                 reference), all (included), ref (the row shows the reference)
//!   sorter:<export folder>        the sorter's own preprocessed data (spikes.rs), drawn without
//!                                 further filtering: its filter, reference and laser cleanup
//!
//! The text is part of the cache key, so each distinct reference is its own cached trace.

use crate::filters::process_from;
use crate::oe::Recording;
use crate::preset::Spec;
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Median,
    Mean,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variant {
    ExcludeOwn,
    IncludeOwn,
    Only,
}

/// A common reference: which channels, how they are combined.
#[derive(Clone, PartialEq, Debug)]
pub struct CarDef {
    pub op: Op,
    pub variant: Variant,
    pub names: Vec<String>,
}

impl CarDef {
    pub fn text(&self) -> String {
        let op = match self.op {
            Op::Median => "median",
            Op::Mean => "mean",
        };
        let v = match self.variant {
            Variant::ExcludeOwn => "excl",
            Variant::IncludeOwn => "all",
            Variant::Only => "ref",
        };
        format!("car:{op}:{v}:{}", self.names.join(","))
    }

    pub fn parse(s: &str) -> Option<CarDef> {
        let mut p = s.strip_prefix("car:")?.splitn(3, ':');
        let op = match p.next()? {
            "median" => Op::Median,
            "mean" => Op::Mean,
            _ => return None,
        };
        let variant = match p.next()? {
            "excl" => Variant::ExcludeOwn,
            "all" => Variant::IncludeOwn,
            "ref" => Variant::Only,
            _ => return None,
        };
        let names = p.next()?.split(',').filter(|s| !s.is_empty()).map(String::from).collect();
        Some(CarDef { op, variant, names })
    }

    /// The same members and combination (rows sharing it can share the members' filtering).
    pub fn members_text(&self) -> String {
        format!("{:?}:{}", self.op, self.names.join(","))
    }

    /// How a row's Ref column shows it.
    pub fn short(&self) -> String {
        let op = if self.op == Op::Median { "median" } else { "mean" };
        match self.variant {
            Variant::Only => format!("CAR itself ({op} of {})", self.names.len()),
            _ => "CAR".into(),
        }
    }
}

pub fn sorter_text(export_dir: &Path) -> String {
    format!("sorter:{}", export_dir.display())
}

/// How the Ref column shows a reference.
pub fn short(reference: &str) -> String {
    if let Some(c) = CarDef::parse(reference) {
        c.short()
    } else if reference.starts_with("sorter:") {
        "sorter".into()
    } else {
        reference.to_string()
    }
}

pub struct Car {
    pub def: CarDef,
    pub members: Vec<usize>,
}

pub enum Ref {
    None,
    Chan(usize),
    Car(Car),
    Sorter(Arc<SorterFile>),
}

impl Ref {
    /// Channels whose recorded data it needs (beyond the row's own).
    pub fn channels(&self) -> Vec<usize> {
        match self {
            Ref::Chan(c) => vec![*c],
            Ref::Car(c) => c.members.clone(),
            _ => vec![],
        }
    }
}

/// The row's reference. An unknown channel in a plain `ref` means none (as before).
pub fn parse(rec: &Recording, spec: &Spec) -> Result<Ref> {
    let Some(r) = spec.reference.as_deref().filter(|r| !r.is_empty()) else { return Ok(Ref::None) };
    if let Some(def) = CarDef::parse(r) {
        let members = def.names.iter().map(|n| rec.ch(n).with_context(|| format!("CAR: unknown channel {n}"))).collect::<Result<Vec<_>>>()?;
        if members.is_empty() {
            bail!("CAR without channels");
        }
        return Ok(Ref::Car(Car { def, members }));
    }
    if let Some(dir) = r.strip_prefix("sorter:") {
        return Ok(Ref::Sorter(SorterFile::open(Path::new(dir))?));
    }
    Ok(rec.ch(r).map_or(Ref::None, Ref::Chan))
}

/// The spec as each CAR member (and the own channel) is filtered: no reference.
fn plain(spec: &Spec) -> Spec {
    Spec { reference: None, ..spec.clone() }
}

/// The spec with its band and notch turned off (the sorter's data is already filtered).
fn unfiltered(spec: &Spec) -> Spec {
    let mut s = plain(spec);
    s.extra.insert("band".into(), serde_json::json!([null, null]));
    s.extra.insert("notch".into(), serde_json::Value::Null);
    s
}

/// Recorded values (physical units) of channel `c` for absolute rows [row, row + out.len()).
pub type Raw<'a> = dyn Fn(usize, usize, &mut [f64]) + Sync + 'a;

/// The processed trace of `spec` (own channel `own`, reference `r`) over rows [first, first + len).
pub fn process(fs: f64, spec: &Spec, r: &Ref, own: usize, first: usize, len: usize, raw: &Raw) -> Result<(Vec<f64>, i64, i64)> {
    match r {
        Ref::None => process_from(len, &|i, out| raw(own, first + i, out), first as i64, fs, spec),
        Ref::Chan(b) => process_from(
            len,
            &|i, out| {
                raw(own, first + i, out);
                let mut w = vec![0.0; out.len()];
                raw(*b, first + i, &mut w);
                out.iter_mut().zip(&w).for_each(|(o, v)| *o -= v);
            },
            first as i64,
            fs,
            spec,
        ),
        Ref::Sorter(f) => {
            let col = f.column(&spec.ch).with_context(|| format!("{} isn't in the sorter's preprocessed data", spec.ch))?;
            process_from(len, &|i, out| f.read(col, first + i, out), first as i64, fs, &unfiltered(spec))
        }
        Ref::Car(car) => {
            let (ys, a, step) = members(fs, spec, car, first, len, raw)?;
            let own_y = own_output(fs, spec, car, own, &ys, first, len, raw)?;
            Ok((combine(car, &ys, own_y.as_ref().map(|(y, k)| (y.as_slice(), *k))), a, step))
        }
    }
}

/// The CAR members, each filtered as `spec` (in parallel); all share one (a, step).
pub fn members(fs: f64, spec: &Spec, car: &Car, first: usize, len: usize, raw: &Raw) -> Result<(Vec<Vec<f64>>, i64, i64)> {
    let p = plain(spec);
    let outs: Vec<(Vec<f64>, i64, i64)> = car
        .members
        .par_iter()
        .map(|&m| process_from(len, &|i, out| raw(m, first + i, out), first as i64, fs, &p))
        .collect::<Result<_>>()?;
    let (a, step) = (outs[0].1, outs[0].2);
    Ok((outs.into_iter().map(|o| o.0).collect(), a, step))
}

/// The own channel's filtered output for a CAR row (with its place among the members), or None
/// for the reference itself.
#[allow(clippy::too_many_arguments)]
pub fn own_output(fs: f64, spec: &Spec, car: &Car, own: usize, ys: &[Vec<f64>], first: usize, len: usize, raw: &Raw) -> Result<Option<(Vec<f64>, Option<usize>)>> {
    if car.def.variant == Variant::Only {
        return Ok(None);
    }
    Ok(Some(match car.members.iter().position(|&m| m == own) {
        Some(k) => (ys[k].clone(), Some(k)),
        None => (process_from(len, &|i, out| raw(own, first + i, out), first as i64, fs, &plain(spec))?.0, None),
    }))
}

/// A row of a CAR group: its variant, and its own filtered wire (with its place among the
/// members), or None for the reference itself.
pub type GroupRow<'a> = (Variant, Option<(&'a [f64], Option<usize>)>);

/// Per sample: own − reference (or the reference itself), the reference being the median or mean
/// of the members (without the own channel for `excl`). numpy's median: the mean of the two
/// middle values for an even count.
pub fn combine(car: &Car, ys: &[Vec<f64>], own: Option<(&[f64], Option<usize>)>) -> Vec<f64> {
    combine_group(car.def.op, ys, &[(car.def.variant, own)]).pop().unwrap()
}

/// `combine` for several rows over the same members at once: each sample's member values are
/// sorted once, and every row's reference read from that (leaving its own value out is a shift
/// of one place), instead of a selection per row.
pub fn combine_group(op: Op, ys: &[Vec<f64>], rows: &[GroupRow]) -> Vec<Vec<f64>> {
    let n = ys.iter().map(Vec::len).min().unwrap_or(0);
    let m = ys.len();
    const PART: usize = 16384;
    let parts: Vec<Vec<Vec<f64>>> = (0..n.div_ceil(PART))
        .into_par_iter()
        .map(|p| {
            let (i0, i1) = (p * PART, ((p + 1) * PART).min(n));
            let mut out: Vec<Vec<f64>> = rows.iter().map(|_| Vec::with_capacity(i1 - i0)).collect();
            let mut sorted = vec![0.0; m];
            for i in i0..i1 {
                for (k, y) in ys.iter().enumerate() {
                    sorted[k] = y[i];
                }
                let sum: f64 = sorted.iter().sum();
                if op == Op::Median {
                    sorted.sort_unstable_by(|a, b| a.total_cmp(b));
                }
                let all = match op {
                    Op::Median => median_sorted(&sorted, None),
                    Op::Mean => if m == 0 { 0.0 } else { sum / m as f64 },
                };
                for (r, (variant, own)) in rows.iter().enumerate() {
                    let reference = match (variant, own) {
                        (Variant::ExcludeOwn, Some((_, Some(k)))) => {
                            let v = ys[*k][i];
                            match op {
                                Op::Median => median_sorted(&sorted, Some(sorted.partition_point(|x| x.total_cmp(&v).is_lt()))),
                                Op::Mean => if m > 1 { (sum - v) / (m - 1) as f64 } else { 0.0 },
                            }
                        }
                        _ => all,
                    };
                    out[r].push(match own {
                        Some((y, _)) => y[i] - reference,
                        None => reference,
                    });
                }
            }
            out
        })
        .collect();
    (0..rows.len()).map(|r| parts.iter().flat_map(|p| p[r].iter().copied()).collect()).collect()
}

/// The median of sorted values, leaving out the one at `skip` (if any).
fn median_sorted(v: &[f64], skip: Option<usize>) -> f64 {
    let len = v.len() - skip.is_some() as usize;
    if len == 0 {
        return 0.0;
    }
    let at = |j: usize| match skip {
        Some(s) if j >= s => v[j + 1],
        _ => v[j],
    };
    if len % 2 == 1 { at(len / 2) } else { (at(len / 2 - 1) + at(len / 2)) / 2.0 }
}

#[cfg(test)]
fn reduce(op: Op, v: &mut [f64]) -> f64 {
    let n = v.len();
    if n == 0 {
        return 0.0;
    }
    match op {
        Op::Mean => v.iter().sum::<f64>() / n as f64,
        Op::Median => {
            let m = n / 2;
            let (lo, mid, _) = v.select_nth_unstable_by(m, |a, b| a.total_cmp(b));
            let mid = *mid;
            if n % 2 == 1 { mid } else { (lo.iter().copied().fold(f64::NEG_INFINITY, f64::max) + mid) / 2.0 }
        }
    }
}

/// The processed trace of `spec` over rows [first, first + len), reading what it needs from the
/// recording (for short views and clips; whole-recording builds read in chunks, cache.rs).
pub fn process_window(rec: &Recording, spec: &Spec, first: usize, len: usize) -> Result<(Vec<f64>, i64, i64)> {
    let own = rec.ch(&spec.ch).with_context(|| format!("no channel {}", spec.ch))?;
    let r = parse(rec, spec)?;
    let mut chans = vec![own];
    for c in r.channels() {
        if !chans.contains(&c) {
            chans.push(c);
        }
    }
    let cols = if matches!(r, Ref::Sorter(_)) { vec![] } else { rec.raw_columns(&chans, first, len) };
    let bv: Vec<f64> = chans.iter().map(|c| rec.bit_volts(*c)).collect();
    let raw = |c: usize, row: usize, out: &mut [f64]| {
        let k = chans.iter().position(|x| *x == c).expect("a channel that was read");
        let u = &cols[k][row - first..row - first + out.len()];
        out.iter_mut().zip(u).for_each(|(o, v)| *o = *v as f64 * bv[k]);
    };
    process(rec.fs, spec, &r, own, first, len, &raw)
}

/// The sorter's preprocessed data (float32, interleaved, the manifest's `preprocessed` block),
/// memory-mapped; rows it doesn't cover read as 0.
pub struct SorterFile {
    map: memmap2::Mmap,
    offset: usize,
    n_channels: usize,
    names: Vec<String>,
    gain: f64,
    start: usize,
    n: usize,
}

impl SorterFile {
    /// Opened once per export folder.
    pub fn open(export_dir: &Path) -> Result<Arc<SorterFile>> {
        static OPEN: OnceLock<Mutex<HashMap<PathBuf, Arc<SorterFile>>>> = OnceLock::new();
        let mut open = OPEN.get_or_init(Default::default).lock().unwrap();
        if let Some(f) = open.get(export_dir) {
            return Ok(f.clone());
        }
        let text = std::fs::read_to_string(export_dir.join(crate::spikes::MANIFEST)).with_context(|| format!("reading the spike export in {}", export_dir.display()))?;
        let m: serde_json::Value = serde_json::from_str(&text)?;
        let p = &m["preprocessed"];
        let path = PathBuf::from(p["path"].as_str().context("the spike export names no preprocessed file")?);
        if p["dtype"].as_str().unwrap_or("float32") != "float32" {
            bail!("the sorter's preprocessed data is {}, not float32", p["dtype"]);
        }
        let names: Vec<String> = p["channel_names"]
            .as_array()
            .or(m["channel_map_names"].as_array())
            .context("the spike export doesn't name the preprocessed file's channels")?
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        let n_channels = p["n_channels"].as_u64().map(|n| n as usize).unwrap_or(names.len());
        let file = std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        // SAFETY: the pipeline writes the file before the manifest and doesn't change it after.
        let map = unsafe { memmap2::Mmap::map(&file)? };
        let offset = p["byte_offset"].as_u64().unwrap_or(0) as usize;
        let n = p["n_samples"].as_u64().map(|n| n as usize).unwrap_or((map.len().saturating_sub(offset)) / (4 * n_channels.max(1)));
        if map.len() < offset + n * n_channels * 4 {
            bail!("{} is shorter than its {} samples × {} channels", path.display(), n, n_channels);
        }
        let f = Arc::new(SorterFile {
            map,
            offset,
            n_channels,
            names,
            gain: p["gain_to_uV"].as_f64().unwrap_or(1.0),
            start: p["start_sample"].as_u64().unwrap_or(0) as usize,
            n,
        });
        open.insert(export_dir.to_path_buf(), f.clone());
        Ok(f)
    }

    pub fn column(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }

    /// Values (µV) of column `col` for absolute rows [row, row + out.len()).
    pub fn read(&self, col: usize, row: usize, out: &mut [f64]) {
        for (i, o) in out.iter_mut().enumerate() {
            let r = row + i;
            *o = if r >= self.start && r < self.start + self.n {
                let at = self.offset + ((r - self.start) * self.n_channels + col) * 4;
                f32::from_le_bytes(self.map[at..at + 4].try_into().unwrap()) as f64 * self.gain
            } else {
                0.0
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn car(op: Op, variant: Variant, n: usize) -> Car {
        Car { def: CarDef { op, variant, names: (0..n).map(|i| format!("CH{i}")).collect() }, members: (0..n).collect() }
    }

    #[test]
    fn text_round_trips() {
        let d = CarDef { op: Op::Median, variant: Variant::ExcludeOwn, names: vec!["CH1".into(), "CH2".into()] };
        assert_eq!(d.text(), "car:median:excl:CH1,CH2");
        assert_eq!(CarDef::parse(&d.text()), Some(d));
        assert_eq!(short("car:mean:ref:CH1,CH2,CH3"), "CAR itself (mean of 3)");
        assert_eq!(short("CH4"), "CH4");
    }

    #[test]
    fn combines_like_numpy() {
        // three members, one sample each: 1, 5, 2; the own channel is member 0
        let ys = vec![vec![1.0], vec![5.0], vec![2.0]];
        let own = Some((ys[0].as_slice(), Some(0)));
        assert_eq!(combine(&car(Op::Median, Variant::IncludeOwn, 3), &ys, own), vec![1.0 - 2.0]);
        assert_eq!(combine(&car(Op::Median, Variant::ExcludeOwn, 3), &ys, own), vec![1.0 - 3.5]); // median of 5, 2
        assert_eq!(combine(&car(Op::Mean, Variant::ExcludeOwn, 3), &ys, own), vec![1.0 - 3.5]);
        assert_eq!(combine(&car(Op::Mean, Variant::Only, 3), &ys, None), vec![8.0 / 3.0]);
        let mut v = vec![4.0, 1.0, 3.0, 2.0];
        assert_eq!(reduce(Op::Median, &mut v), 2.5);
    }

    #[test]
    fn group_matches_one_at_a_time() {
        // 16 members (with ties), 2000 samples; every row excluding its own wire, plus "all" and "ref"
        let mut x = 12345u64;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            ((x % 41) as f64 - 20.0) * 0.5
        };
        let ys: Vec<Vec<f64>> = (0..16).map(|_| (0..2000).map(|_| rnd()).collect()).collect();
        for op in [Op::Median, Op::Mean] {
            let mut rows: Vec<GroupRow> = (0..16).map(|k| (Variant::ExcludeOwn, Some((ys[k].as_slice(), Some(k))))).collect();
            rows.push((Variant::IncludeOwn, Some((ys[3].as_slice(), Some(3)))));
            rows.push((Variant::Only, None));
            let got = combine_group(op, &ys, &rows);
            for (r, (variant, own)) in rows.iter().enumerate() {
                for i in 0..2000 {
                    let mut vals: Vec<f64> = (0..16).filter(|k| !(*variant == Variant::ExcludeOwn && own.unwrap().1 == Some(*k))).map(|k| ys[k][i]).collect();
                    let reference = reduce(op, &mut vals);
                    let want = own.map_or(reference, |(y, _)| y[i] - reference);
                    assert!((got[r][i] - want).abs() < 1e-9, "{op:?} row {r} sample {i}: {} vs {want}", got[r][i]);
                }
            }
        }
    }
}
