//! Dimna recordings (format v1, written by the Dimna acquisition program): one folder holding
//!
//!   samples.i16      int16 LE, [n_samples][num_channels], raw as acquired (value × bit_volts)
//!   index.bin        48-byte LE records, one per ~100 ms chunk (first_sample i64, n_samples u32,
//!                    flags u32 (bit 0: samples were lost before this chunk), byte_offset u64,
//!                    wall_ns i64, board_clock u64, crc32 u32, pad u32)
//!   ttl_sample.i64   board sample number of each TTL-input change (first entry: initial state)
//!   ttl_word.u16     the 8-line input word from that sample on (bit k = line k + 1)
//!   events.tsv       sample<TAB>kind<TAB>text
//!   meta.toml        channels, scales, settings; [final] only after a clean stop
//!
//! Only rows covered by index.bin are read (the recorder syncs samples before their index
//! records, so these are on disk); a tail past them, after a crash, is left out. Dimna's
//! `tools/dimna_read.py` is the reference reader.
//!
//! A Dimna session is `<session>/ephys/` (this format) beside `<session>/video/`; the session
//! folder is where syncview finds the videos and keeps its annotations.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const META: &str = "meta.toml";
const FORMAT_NAMES: [&str; 2] = ["dimna-recording", "oez-recording"];
const INDEX_RECORD: usize = 48;
const FLAG_GAP: u32 = 1;

#[derive(Deserialize)]
struct Meta {
    format: String,
    format_version: i64,
    byte_order: Option<String>,
    sample_rate: f64,
    num_channels: usize,
    created: Option<toml::value::Datetime>,
    channels: Channels,
    ttl: Option<Ttl>,
    start: Option<Start>,
    #[serde(rename = "final")]
    fin: Option<toml::Value>,
}

#[derive(Deserialize)]
struct Channels {
    name: Vec<String>,
    kind: Vec<String>,
    units: Vec<String>,
    bit_volts: Vec<f64>,
}

#[derive(Deserialize)]
struct Ttl {
    #[serde(default)]
    line_names: Vec<String>,
}

#[derive(Deserialize)]
struct Start {
    first_sample: i64,
}

/// A chunk of rows with consecutive sample numbers (chunks without a gap between them are merged).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Run {
    first_sample: i64,
    row: usize,
    n: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub sample: i64,
    pub kind: String,
    pub text: String,
}

pub struct Dimna {
    pub dir: PathBuf,
    pub fs: f64,
    pub n_channels: usize,
    pub names: Vec<String>,
    /// "electrode", "aux" or "adc"
    pub kinds: Vec<String>,
    pub units: Vec<String>,
    pub bit_volts: Vec<f64>,
    /// Rows covered by the index (the rows syncview reads).
    pub n_samples: usize,
    pub first_sample: i64,
    /// A clean stop wrote [final]; false: interrupted, or still being recorded.
    pub complete: bool,
    /// Bytes of samples.i16 past the last index record (unverified, not read).
    pub tail_bytes: u64,
    pub created: Option<String>,
    /// What each TTL line carries, line 1 first ("" = not named).
    #[allow(dead_code)] // for picking the camera line (not yet used)
    pub line_names: Vec<String>,
    pub events: Vec<Event>,
    runs: Vec<Run>,
    /// Board sample numbers of the rising edges, per line (1..=8).
    rising: BTreeMap<u16, Vec<i64>>,
}

/// A folder (or its meta.toml) holding a Dimna recording.
pub fn is_recording(p: &Path) -> bool {
    let dir = if p.is_dir() {
        p
    } else if p.file_name().is_some_and(|n| n == META) {
        match p.parent() {
            Some(d) => d,
            None => return false,
        }
    } else {
        return false;
    };
    dir.join("samples.i16").is_file()
        && std::fs::read_to_string(dir.join(META)).is_ok_and(|s| {
            s.lines().any(|l| {
                let Some((k, v)) = l.split_once('=') else {
                    return false;
                };
                k.trim() == "format" && FORMAT_NAMES.contains(&v.trim().trim_matches('"'))
            })
        })
}

/// The recording folder for what was given: the folder itself, its `meta.toml`, or a session
/// folder holding `ephys/`. None if it's none of these.
/// Links are resolved (a session opened through `current` is that session, not the next one).
pub fn locate(p: &Path) -> Option<PathBuf> {
    let found = if p.is_file() && is_recording(p) {
        p.parent().map(Path::to_path_buf)
    } else if p.is_dir() && is_recording(p) {
        Some(p.to_path_buf())
    } else if p.is_dir() && is_recording(&p.join("ephys")) {
        Some(p.join("ephys"))
    } else {
        None
    }?;
    Some(std::fs::canonicalize(&found).unwrap_or(found))
}

/// The session folder of a recording at `<session>/ephys/`.
pub fn session_of(rec_dir: &Path) -> Option<PathBuf> {
    (rec_dir.file_name().is_some_and(|n| n == "ephys") && is_recording(rec_dir))
        .then(|| rec_dir.parent().map(Path::to_path_buf))
        .flatten()
}

fn read_le<const N: usize, T>(path: &Path, f: impl Fn([u8; N]) -> T) -> Result<Vec<T>> {
    let b = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    Ok(b.chunks_exact(N)
        .map(|c| f(c.try_into().unwrap()))
        .collect())
}

impl Dimna {
    pub fn open(dir: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(dir.join(META))
            .with_context(|| format!("reading {}", dir.join(META).display()))?;
        let m: Meta = toml::from_str(&text).with_context(|| {
            format!(
                "{} is not a Dimna recording's meta.toml",
                dir.join(META).display()
            )
        })?;
        if !FORMAT_NAMES.contains(&m.format.as_str()) || m.format_version != 1 {
            bail!(
                "{}: format {:?} version {} (syncview reads dimna-recording version 1)",
                dir.display(),
                m.format,
                m.format_version
            );
        }
        if m.byte_order.as_deref().is_some_and(|b| b != "little") {
            bail!(
                "{}: byte order {:?} is not supported",
                dir.display(),
                m.byte_order
            );
        }
        let nc = m.num_channels;
        let c = &m.channels;
        if nc == 0
            || [c.name.len(), c.kind.len(), c.units.len(), c.bit_volts.len()]
                .iter()
                .any(|&n| n != nc)
        {
            bail!(
                "{}: meta.toml lists {} channels but the [channels] lists have other lengths",
                dir.display(),
                nc
            );
        }
        if m.sample_rate.is_nan() || m.sample_rate <= 0.0 {
            bail!("{}: sample rate {}", dir.display(), m.sample_rate);
        }

        // index.bin -> runs of consecutive sample numbers
        let index = std::fs::read(dir.join("index.bin"))
            .with_context(|| format!("reading {}", dir.join("index.bin").display()))?;
        let file_len = std::fs::metadata(dir.join("samples.i16"))
            .with_context(|| format!("reading {}", dir.join("samples.i16").display()))?
            .len();
        let mut runs: Vec<Run> = vec![];
        let mut rows = 0usize;
        let mut covered = 0u64;
        for rec in index.chunks_exact(INDEX_RECORD) {
            let first = i64::from_le_bytes(rec[0..8].try_into().unwrap());
            let n = u32::from_le_bytes(rec[8..12].try_into().unwrap()) as usize;
            let flags = u32::from_le_bytes(rec[12..16].try_into().unwrap());
            let off = u64::from_le_bytes(rec[16..24].try_into().unwrap());
            let end = off + (n * nc * 2) as u64;
            if off != covered || end > file_len {
                // never expected (the recorder writes them in order, behind the data): stop here
                eprintln!(
                    "syncview: {}: index.bin record for sample {first} doesn't follow the previous one; reading up to it",
                    dir.display()
                );
                break;
            }
            match runs.last_mut() {
                Some(r) if flags & FLAG_GAP == 0 && r.first_sample + r.n as i64 == first => {
                    r.n += n
                }
                _ => runs.push(Run {
                    first_sample: first,
                    row: rows,
                    n,
                }),
            }
            rows += n;
            covered = end;
        }
        let first_sample = m
            .start
            .as_ref()
            .map(|s| s.first_sample)
            .or(runs.first().map(|r| r.first_sample))
            .unwrap_or(0);

        // TTL: rising edges per line (the first entry is the initial state, not an edge)
        let samples = read_le(&dir.join("ttl_sample.i64"), i64::from_le_bytes)?;
        let words = read_le(&dir.join("ttl_word.u16"), u16::from_le_bytes)?;
        let mut rising: BTreeMap<u16, Vec<i64>> = BTreeMap::new();
        let mut prev: Option<u16> = None;
        for (&s, &w) in samples.iter().zip(&words) {
            if let Some(p) = prev {
                let up = w & !p;
                for line in 1..=8u16 {
                    if up >> (line - 1) & 1 == 1 {
                        rising.entry(line).or_default().push(s);
                    }
                }
            }
            prev = Some(w);
        }
        // lines that changed at all are listed (empty: no rising edge)
        if let Some(&w0) = words.first() {
            let changed = words.iter().fold(0u16, |acc, w| acc | (w ^ w0));
            for line in 1..=8u16 {
                if changed >> (line - 1) & 1 == 1 {
                    rising.entry(line).or_default();
                }
            }
        }

        let events = std::fs::read_to_string(dir.join("events.tsv"))
            .map(|s| {
                s.lines()
                    .skip(1)
                    .filter_map(|l| {
                        let mut f = l.splitn(3, '\t');
                        let sample = f.next()?.trim().parse().ok()?;
                        Some(Event {
                            sample,
                            kind: f.next()?.to_string(),
                            text: f.next().unwrap_or("").to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(Dimna {
            dir: dir.to_path_buf(),
            fs: m.sample_rate,
            n_channels: nc,
            names: m.channels.name,
            kinds: m.channels.kind,
            units: m.channels.units,
            bit_volts: m.channels.bit_volts,
            n_samples: rows,
            first_sample,
            complete: m.fin.is_some(),
            tail_bytes: file_len - covered,
            created: m.created.map(|d| d.to_string()),
            line_names: m.ttl.map(|t| t.line_names).unwrap_or_default(),
            events,
            runs,
            rising,
        })
    }

    pub fn samples_path(&self) -> PathBuf {
        self.dir.join("samples.i16")
    }

    pub fn is_contiguous(&self) -> bool {
        self.runs.len() <= 1
    }

    /// Open Ephys's channel type codes: 0 electrode, 1 aux, 2 adc.
    pub fn type_codes(&self) -> Vec<Option<i64>> {
        self.kinds
            .iter()
            .map(|k| match k.as_str() {
                "electrode" => Some(0),
                "aux" => Some(1),
                "adc" => Some(2),
                _ => None,
            })
            .collect()
    }

    /// Row of a board sample number: inside a gap, the first row after it; outside the
    /// recording, extrapolated (as oe::Recording::row_of does for Open Ephys).
    pub fn row_of(&self, sn: i64) -> i64 {
        let Some(first) = self.runs.first() else {
            return sn - self.first_sample;
        };
        let i = self.runs.partition_point(|r| r.first_sample <= sn);
        if i == 0 {
            return sn - first.first_sample;
        }
        let r = self.runs[i - 1];
        let off = sn - r.first_sample;
        if off < r.n as i64 || i == self.runs.len() {
            r.row as i64 + off
        } else {
            self.runs[i].row as i64
        }
    }

    /// Rows of the rising edges, per TTL line.
    pub fn rising_rows(&self) -> BTreeMap<u16, Vec<i64>> {
        self.rising
            .iter()
            .map(|(l, v)| (*l, v.iter().map(|s| self.row_of(*s)).collect()))
            .collect()
    }

    #[allow(dead_code)]
    /// The TTL line named like a camera ("camera", "cam", "video", "frame"), if any.
    pub fn camera_line(&self) -> Option<i64> {
        self.line_names
            .iter()
            .position(|n| {
                let n = n.to_lowercase();
                ["cam", "video", "frame", "basler"]
                    .iter()
                    .any(|w| n.contains(w))
            })
            .map(|i| i as i64 + 1)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Writes a small Dimna recording: `chunks` of (first sample, n, gap flag); sample value =
    /// row * 10 + channel; TTL (sample, word) changes; `fin`: a [final] section; `tail` extra
    /// unindexed rows.
    pub fn write(
        dir: &Path,
        nc: usize,
        chunks: &[(i64, u32, bool)],
        ttl: &[(i64, u16)],
        fin: bool,
        tail: usize,
    ) {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        let mut samples = vec![];
        let mut index = vec![];
        let mut row = 0i64;
        for &(first, n, gap) in chunks {
            let off = samples.len() as u64;
            let mut bytes = vec![];
            for _ in 0..n {
                for ch in 0..nc {
                    bytes.extend_from_slice(&((row * 10 + ch as i64) as i16).to_le_bytes());
                }
                row += 1;
            }
            index.extend_from_slice(&first.to_le_bytes());
            index.extend_from_slice(&n.to_le_bytes());
            index.extend_from_slice(&u32::from(gap).to_le_bytes());
            index.extend_from_slice(&off.to_le_bytes());
            index.extend_from_slice(&0i64.to_le_bytes());
            index.extend_from_slice(&0u64.to_le_bytes());
            index.extend_from_slice(&0u32.to_le_bytes()); // crc32: not checked by syncview
            index.extend_from_slice(&0u32.to_le_bytes());
            samples.extend(bytes);
        }
        samples.extend(std::iter::repeat_n(0u8, tail * nc * 2));
        std::fs::write(dir.join("samples.i16"), samples).unwrap();
        std::fs::write(dir.join("index.bin"), index).unwrap();
        std::fs::write(
            dir.join("ttl_sample.i64"),
            ttl.iter()
                .flat_map(|t| t.0.to_le_bytes())
                .collect::<Vec<u8>>(),
        )
        .unwrap();
        std::fs::write(
            dir.join("ttl_word.u16"),
            ttl.iter()
                .flat_map(|t| t.1.to_le_bytes())
                .collect::<Vec<u8>>(),
        )
        .unwrap();
        std::fs::write(
            dir.join("events.tsv"),
            format!(
                "sample\tkind\ttext\n{}\tstart\t\n{}\tnote\tlever down\n",
                chunks[0].0,
                chunks[0].0 + 5
            ),
        )
        .unwrap();
        let q = |v: Vec<String>| {
            v.iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let names: Vec<String> = (0..nc)
            .map(|i| {
                if i + 1 < nc {
                    format!("CH{}", i + 1)
                } else {
                    "ADC1".into()
                }
            })
            .collect();
        let kinds = (0..nc)
            .map(|i| if i + 1 < nc { "electrode" } else { "adc" }.to_string())
            .collect();
        let units = (0..nc)
            .map(|i| if i + 1 < nc { "uV" } else { "V" }.to_string())
            .collect();
        let bv = (0..nc)
            .map(|i| if i + 1 < nc { "0.195" } else { "0.00015258789" })
            .collect::<Vec<_>>()
            .join(", ");
        let mut meta = format!(
            "format = \"dimna-recording\"\nformat_version = 1\nbyte_order = \"little\"\ncreated = 2026-11-21T05:34:59-05:00\nsample_rate = 1000.0\nnum_channels = {nc}\n\n[channels]\nname = [{}]\nkind = [{}]\nunits = [{}]\nbit_volts = [{bv}]\n\n[ttl]\nline_names = [\"\", \"Basler camera\", \"\", \"\", \"\", \"\", \"\", \"\"]\n\n[start]\nfirst_sample = {}\n",
            q(names),
            q(kinds),
            q(units),
            chunks[0].0
        );
        if fin {
            meta += "\n[final]\nsamples = 0\n";
        }
        std::fs::write(dir.join(META), meta).unwrap();
    }

    #[test]
    fn reads_rows_gaps_and_ttl() {
        let d = std::env::temp_dir().join(format!("syncview_dimna_test_{}", std::process::id()));
        // samples 100..150, a gap, 200..230 (two chunks merged), and an unindexed tail
        write(
            &d,
            3,
            &[(100, 50, false), (200, 20, true), (220, 10, false)],
            &[
                (100, 0b00),
                (110, 0b10),
                (120, 0b00),
                (130, 0b11),
                (205, 0b01),
                (210, 0b11),
            ],
            false,
            7,
        );
        assert!(is_recording(&d) && is_recording(&d.join(META)));
        assert_eq!(locate(&d.join(META)), Some(d.clone()));
        let r = Dimna::open(&d).unwrap();
        assert_eq!(
            (r.n_samples, r.first_sample, r.complete, r.tail_bytes),
            (80, 100, false, 7 * 3 * 2)
        );
        assert_eq!(r.runs.len(), 2);
        assert!(!r.is_contiguous());
        assert_eq!(r.type_codes(), vec![Some(0), Some(0), Some(2)]);
        // rows: inside runs, in the gap (-> first row after it), before and after the recording
        assert_eq!(
            [
                r.row_of(100),
                r.row_of(149),
                r.row_of(170),
                r.row_of(200),
                r.row_of(229),
                r.row_of(240),
                r.row_of(90)
            ],
            [0, 49, 50, 50, 79, 90, -10]
        );
        // line 1 rises at 130; line 2 at 110, 130 and 210 (it falls at 205)
        let rows = r.rising_rows();
        assert_eq!(rows[&1], vec![30]);
        assert_eq!(rows[&2], vec![10, 30, 50 + 10]);
        assert_eq!(r.camera_line(), Some(2));
        assert_eq!(r.events.len(), 2);
        assert_eq!(
            r.events[1],
            Event {
                sample: 105,
                kind: "note".into(),
                text: "lever down".into()
            }
        );
        // a session folder: <session>/ephys
        let s = d.with_extension("session");
        let _ = std::fs::remove_dir_all(&s);
        write(&s.join("ephys"), 2, &[(0, 10, false)], &[], true, 0);
        assert_eq!(locate(&s), Some(s.join("ephys")));
        assert_eq!(session_of(&s.join("ephys")), Some(s.clone()));
        assert_eq!(session_of(&d), None);
        assert!(Dimna::open(&s.join("ephys")).unwrap().complete);
        // through the viewer's recording: sizes, rows' values (row * 10 + channel), edges, home
        let rec = crate::oe::Recording::open(&s, None).unwrap();
        assert_eq!(
            (rec.rec_dir.clone(), rec.home.clone()),
            (
                std::fs::canonicalize(s.join("ephys")).unwrap(),
                std::fs::canonicalize(&s).unwrap()
            )
        );
        let rec = crate::oe::Recording::open(&d, None).unwrap();
        assert_eq!(
            (
                rec.n_samples(),
                rec.first_sample_number(),
                rec.is_contiguous(),
                rec.ch("ADC1")
            ),
            (80, 100, false, Some(2))
        );
        assert_eq!(rec.ch_types, vec![Some(0), Some(0), Some(2)]);
        let cols = rec.raw_columns(&[2, 0], 45, 30);
        assert_eq!(
            cols[0],
            (45..75).map(|r| (r * 10 + 2) as i16).collect::<Vec<_>>()
        );
        assert_eq!(
            cols[1],
            (45..75).map(|r| (r * 10) as i16).collect::<Vec<_>>()
        );
        assert_eq!(rec.rising_edges(2), vec![10, 30, 60]);
        assert_eq!(rec.bit_volts(2), 0.00015258789);
        std::fs::remove_dir_all(&d).unwrap();
        std::fs::remove_dir_all(&s).unwrap();
    }
}
