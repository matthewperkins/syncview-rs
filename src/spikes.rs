//! Sorted spikes from a spike export (the phy/Kilosort layout plus `syncview_spikes.json`), written
//! by the sorting pipeline into a folder in the recording folder:
//!
//!   spike_times.npy        (n,) int: sample index from the recording's first sample (as annotations)
//!   spike_clusters.npy     (n,) int: unit id
//!   cluster_info.tsv       one row per unit: cluster_id, wire (channel name), class, fr, snr, … (all kept)
//!   laser_windows.npy      (k, 2) int: blanked [start, end) samples (optional)
//!   syncview_spikes.json   the manifest, written last: recording identity, wires, preprocessing, …
//!
//! (templates, amplitudes and per-spike diagnostics are read by later features.)

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const MANIFEST: &str = "syncview_spikes.json";

/// Colours for units without one in cluster_info.tsv (matplotlib's tab10, which the pipeline uses).
const PALETTE: [[u8; 3]; 10] = [
    [0x1f, 0x77, 0xb4], [0xff, 0x7f, 0x0e], [0x2c, 0xa0, 0x2c], [0xd6, 0x27, 0x28], [0x94, 0x67, 0xbd],
    [0x8c, 0x56, 0x4b], [0xe3, 0x77, 0xc2], [0x7f, 0x7f, 0x7f], [0xbc, 0xbd, 0x22], [0x17, 0xbe, 0xcf],
];

pub struct Unit {
    pub id: i64,
    pub wire: String,
    pub class: String,
    pub color: [u8; 3],
    /// Its row in templates.npy: the template_row column, else its place in cluster_info.tsv
    /// (never its cluster id, as phy would have it).
    #[allow(dead_code)] // (the waveform box will read templates)
    pub template_row: usize,
    /// Spike sample indexes, ascending.
    pub times: Vec<i64>,
    /// Every cluster_info.tsv column, as written (header, value).
    pub info: Vec<(String, String)>,
}

impl Unit {
    pub fn tagged(&self) -> bool {
        self.class.eq_ignore_ascii_case("tagged")
    }

    pub fn get(&self, col: &str) -> Option<&str> {
        self.info.iter().find(|(k, _)| k == col).map(|(_, v)| v.as_str())
    }

    /// Indexes into `times` of the spikes in samples [a, b).
    pub fn range(&self, a: i64, b: i64) -> std::ops::Range<usize> {
        self.times.partition_point(|&s| s < a)..self.times.partition_point(|&s| s < b)
    }
}

pub struct Spikes {
    pub dir: PathBuf,
    pub manifest: Value,
    pub units: Vec<Unit>,
    /// Blanked (laser) windows, [start, end) samples.
    pub laser: Vec<[i64; 2]>,
    /// A test export covering only samples [start, end).
    pub subset: Option<(i64, i64)>,
    /// The brain wires (spikes and references come from these; EMG wires never go into a reference).
    pub brain: Vec<String>,
}

impl Spikes {
    pub fn name(&self) -> String {
        self.dir.file_name().unwrap_or_default().to_string_lossy().into_owned()
    }

    pub fn n_spikes(&self) -> usize {
        self.units.iter().map(|u| u.times.len()).sum()
    }

    pub fn has_wire(&self, wire: &str) -> bool {
        self.units.iter().any(|u| u.wire == wire)
    }
}

/// The spike exports in a recording folder: `<rec>/<folder>/syncview_spikes.json` and
/// `<rec>/<folder>/<name>/syncview_spikes.json` (several sortings), full exports first.
pub fn find(rec_dir: &Path) -> Vec<PathBuf> {
    let mut found = vec![];
    let subdirs = |d: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        v.sort();
        v
    };
    for d in subdirs(rec_dir) {
        if d.join(MANIFEST).is_file() {
            found.push(d);
        } else if d.file_name().is_some_and(|n| n.to_string_lossy().starts_with("spikes")) {
            found.extend(subdirs(&d).into_iter().filter(|s| s.join(MANIFEST).is_file()));
        }
    }
    let is_test = |d: &PathBuf| {
        std::fs::read_to_string(d.join(MANIFEST)).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).is_some_and(|m| m["subset"].is_object())
    };
    found.sort_by_key(is_test);
    found
}

/// Read an export, checking that it belongs to the recording (sample rate, length, first sample).
pub fn load(dir: &Path, fs: f64, n_samples: usize, first_sample_number: i64) -> Result<Spikes> {
    let text = std::fs::read_to_string(dir.join(MANIFEST)).with_context(|| format!("reading {}", dir.join(MANIFEST).display()))?;
    let m: Value = serde_json::from_str(&text).with_context(|| format!("{MANIFEST} isn't valid JSON"))?;
    let version = m["syncview_spikes"].as_u64().unwrap_or(0);
    if version != 1 {
        bail!("{MANIFEST} is format {version}; this syncview reads format 1");
    }
    let r = &m["recording"];
    let (efs, en, efirst) = (r["sample_rate"].as_f64(), r["n_samples"].as_u64(), r["first_sample_number"].as_i64());
    if efs.is_some_and(|x| (x - fs).abs() > 1e-6) || en.is_some_and(|x| x as usize != n_samples) || efirst.is_some_and(|x| x != first_sample_number) {
        bail!(
            "this export is for a different recording: it says {} samples at {} Hz from sample number {}, the recording has {} at {} Hz from {}",
            en.map_or("?".into(), |x| x.to_string()),
            efs.map_or("?".into(), |x| x.to_string()),
            efirst.map_or("?".into(), |x| x.to_string()),
            n_samples,
            fs,
            first_sample_number
        );
    }
    let times = crate::npy::read_i64(&dir.join("spike_times.npy"))?;
    let clusters = crate::npy::read_i64(&dir.join("spike_clusters.npy"))?;
    if times.len() != clusters.len() {
        bail!("spike_times.npy has {} spikes but spike_clusters.npy {}", times.len(), clusters.len());
    }
    if let Some(&s) = times.iter().find(|&&s| s < 0 || s as usize >= n_samples) {
        bail!("spike_times.npy has sample {s}, outside the recording (0 to {})", n_samples - 1);
    }
    // units, in cluster_info.tsv order
    let tsv = std::fs::read_to_string(dir.join("cluster_info.tsv")).context("reading cluster_info.tsv")?;
    let mut lines = tsv.lines().filter(|l| !l.trim().is_empty());
    let header: Vec<String> = lines.next().context("cluster_info.tsv is empty")?.split('\t').map(|s| s.trim().to_string()).collect();
    let col = |name: &str| header.iter().position(|h| h == name);
    let (Some(ci), Some(wi)) = (col("cluster_id").or(col("id")), col("wire")) else {
        bail!("cluster_info.tsv needs cluster_id and wire columns");
    };
    let mut units = vec![];
    for line in lines {
        let f: Vec<&str> = line.split('\t').collect();
        let id: i64 = f.get(ci).and_then(|s| s.trim().parse().ok()).with_context(|| format!("cluster_info.tsv: bad cluster_id in {line:?}"))?;
        let info: Vec<(String, String)> = header.iter().enumerate().map(|(i, h)| (h.clone(), f.get(i).unwrap_or(&"").trim().to_string())).collect();
        let color = col("colour").or(col("color")).and_then(|i| f.get(i)).and_then(|c| parse_hex(c)).unwrap_or(PALETTE[units.len() % PALETTE.len()]);
        units.push(Unit {
            id,
            wire: f.get(wi).unwrap_or(&"").trim().to_string(),
            class: col("class").and_then(|i| f.get(i)).unwrap_or(&"").trim().to_string(),
            color,
            template_row: col("template_row").and_then(|i| f.get(i)).and_then(|s| s.trim().parse().ok()).unwrap_or(units.len()),
            times: vec![],
            info,
        });
    }
    let mut missing = std::collections::BTreeSet::new();
    for (&t, &c) in times.iter().zip(&clusters) {
        match units.iter_mut().find(|u| u.id == c) {
            Some(u) => u.times.push(t),
            None => {
                missing.insert(c);
            }
        }
    }
    if !missing.is_empty() {
        eprintln!("syncview: {}: spikes of clusters {missing:?} have no row in cluster_info.tsv; they aren't shown", dir.display());
    }
    for u in &mut units {
        if !u.times.is_sorted() {
            u.times.sort_unstable();
        }
    }
    let laser = match crate::npy::read_i64(&dir.join("laser_windows.npy")) {
        Ok(v) => v.chunks_exact(2).map(|w| [w[0], w[1]]).collect(),
        Err(_) => vec![],
    };
    let subset = m["subset"].as_object().map(|s| (s["start_sample"].as_i64().unwrap_or(0), s["end_sample"].as_i64().unwrap_or(n_samples as i64)));
    let brain = m["wires"]
        .as_array()
        .map(|w| w.iter().filter(|w| w["kind"] == "brain").filter_map(|w| w["name"].as_str().map(String::from)).collect())
        .unwrap_or_else(|| units.iter().map(|u| u.wire.clone()).collect());
    Ok(Spikes { dir: dir.to_path_buf(), manifest: m, units, laser, subset, brain })
}

fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small export written the way the pipeline writes one.
    fn write_example(dir: &Path, n_samples: usize) {
        std::fs::create_dir_all(dir).unwrap();
        crate::npy::write_i64(&dir.join("spike_times.npy"), &[100, 200, 250, 900]).unwrap();
        crate::npy::write_i64(&dir.join("spike_clusters.npy"), &[3, 7, 3, 3]).unwrap();
        std::fs::write(dir.join("cluster_info.tsv"), "cluster_id\twire\tclass\tfr\tcolour\n3\tCH1\tTAGGED\t2.5\t#ff0000\n7\tCH2\tcandidate\t1\t\n").unwrap();
        // (no template_row column: rows follow the table)
        let m = serde_json::json!({
            "syncview_spikes": 1,
            "recording": {"sample_rate": 30000.0, "n_samples": n_samples, "first_sample_number": 5},
            "wires": [{"name": "CH1", "kind": "brain"}, {"name": "CH2", "kind": "brain"}, {"name": "CH3", "kind": "emg"}],
        });
        std::fs::write(dir.join(MANIFEST), m.to_string()).unwrap();
    }

    #[test]
    fn loads_an_export() {
        let root = std::env::temp_dir().join(format!("syncview_spikes_test_{}", std::process::id()));
        let dir = root.join("spikes_phy");
        write_example(&dir, 1000);
        assert_eq!(find(&root), vec![dir.clone()]);
        let s = load(&dir, 30000.0, 1000, 5).unwrap();
        assert_eq!(s.units.len(), 2);
        assert_eq!((s.units[0].id, s.units[0].times.clone(), s.units[0].color, s.units[0].tagged()), (3, vec![100, 250, 900], [255, 0, 0], true));
        assert_eq!(s.units[1].times, vec![200]);
        assert_eq!((s.units[0].template_row, s.units[1].template_row), (0, 1));
        assert_eq!(s.units[0].range(200, 901), 1..3);
        assert_eq!(s.brain, vec!["CH1", "CH2"]);
        assert!(s.subset.is_none() && s.laser.is_empty());
        // another recording's export is refused
        assert!(load(&dir, 30000.0, 2000, 5).err().unwrap().to_string().contains("different recording"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
