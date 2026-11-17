# Spike export format (read by syncview)

What a spike sorting pipeline writes for syncview to show sorted spikes: the phy/Kilosort files
plus a manifest. syncview is written in Rust and reads `.npy`, TSV and JSON; it never runs Python.

## Where

One folder per sorted recording, named `spikes_phy/` and placed in the Open Ephys recording folder (the
one with `structure.oebin`), next to `syncview_annotations.csv`. If several sortings exist, use
`spikes_phy/<sorting name>/`. The large preprocessed file can stay in the analysis folder; the manifest
points to it by absolute path. (Location confirmed with Matthew.)

## Files

Keep your table, including the extras and `laser_windows.npy`, with these details:

- `spike_times.npy`: int64 or uint64, sorted ascending. Sample 0 is the first sample of that
  recording's `continuous.dat`, the convention of `start_sample` in `syncview_annotations.csv`. If
  several recordings were concatenated for sorting, split them so each export is in its own recording's
  clock.
- `spike_clusters.npy`, `amplitudes.npy` and the extras: one value per spike, the same length and order
  as `spike_times.npy`.
- `templates.npy`: in µV, **not whitened**, channels in `channel_map.npy` order.
- **New** `templates_std.npy`: the same shape, the per-sample SD (for the ± SD shading). Optional;
  without it syncview computes it.
- `cluster_info.tsv`: tab-separated with a header. Required columns: `cluster_id` (matching
  `spike_clusters.npy`), `wire` (the Open Ephys channel name, e.g. `CH12`), `class`, `n_spikes`, `fr`,
  `snr`, `pickup_score`. `colour` (`#rrggbb`) is optional. Any extra columns are shown as they are.
- `params.py`: keep it for phy, but syncview reads the manifest instead.

## New: `syncview_spikes.json` (the manifest)

Write it last: its presence marks a complete export.

```json
{
  "syncview_spikes": 1,
  "created": "2026-11-17T15:00:00Z",
  "created_by": "OEanalysis_with_claude <git hash>",
  "sorter": {"name": "mountainsort5", "version": "0.5.x", "spikeinterface": "0.10x", "params": {}},
  "recording": {
    "path": "/abs/path/to/Record Node 101/experiment1/recording1",
    "stream": "acquisition_board",
    "sample_rate": 30000.0,
    "n_samples": 324000000,
    "first_sample_number": 123456
  },
  "wires": [
    {"name": "CH1", "kind": "brain", "good": true},
    {"name": "CH17", "kind": "emg", "muscle": "digastric", "good": true}
  ],
  "channel_map_names": ["CH1", "CH2", "CH4"],
  "preprocessing": {
    "bandpass_hz": [300, 6000], "filter": "butterworth order 5, zero-phase (or as used)",
    "reference": {"operator": "median", "wires": ["CH1", "CH2", "CH4"], "includes_own_wire": true},
    "laser": {"template_ms": [-1, 3], "zeroed_ms": [-0.2, 1]}
  },
  "preprocessed": {
    "path": "/abs/path/preprocessed/traces_cached_seg0.raw",
    "dtype": "float32", "n_channels": 13, "channel_names": ["CH1", "CH2", "CH4"],
    "units": "uV", "byte_offset": 0
  },
  "templates": {"n_before": 45, "n_samples": 90},
  "extras": {
    "spike_template_corr": {"unit": "r", "suspect": "below", "default_threshold": 0.8},
    "spike_ref_amp": {"unit": "uV", "suspect": "above"},
    "spike_masseter_env": {"unit": "x noise floor", "suspect": "above", "default_threshold": 3}
  }
}
```

The JSON lists are shortened. `wires` covers every channel in the recording, brain and EMG. syncview
uses it to keep EMG wires out of any reference and to grey out bad wires. `channel_map_names` gives the
wire name for each column of the templates and the preprocessed file. `first_sample_number` and
`n_samples` let syncview refuse an export that doesn't belong to the recording it is opening.

Two things to confirm: that the preprocessed file is in µV, and that it has exactly `n_samples` rows.
If it is shorter, say where it starts.

## Reading back

syncview writes `syncview_unit_verdicts.csv` in the export folder, one row per verdict, with the latest
row per unit taking effect:

```csv
cluster_id,verdict,note,annotator,time_utc
15,pickup,fires only in masseter bursts,MP,2026-11-17T16:02:11Z
```

`verdict` is one of `neuron`, `pickup` or `unsure`.

## Checks before handing over

- `spike_times` is sorted and lies within 0 to `n_samples` − 1. All per-spike arrays have the same
  length. Every cluster in `spike_clusters` is in `cluster_info.tsv`.
- Each unit's template peaks on its `wire`.
- Print, for u8 and u15, the first three spike times in samples and in seconds. Matthew can then check
  that syncview lands on the same spikes.
- Also write a **small test export**: the first 60 s of one session with a few units, including a pickup
  case like u6. syncview's tests can run on it.

## Not needed

PC features, whitened data, per-spike snippets, or the phy curation UI.

## Notes from the first exporter (2026-11-17)

- A test export (`subset` block in the manifest, not null) covers only samples
  `start_sample`–`end_sample`; it may carry its own short copy of the preprocessed data, described by
  `preprocessed.start_sample` and `preprocessed.n_samples`.
- The preprocessed file may be in raw ADC counts: `preprocessed.gain_to_uV` converts it.
- A unit's rows in `templates.npy` and `templates_std.npy` are given by the `template_row` column of
  `cluster_info.tsv` (and the manifest's `templates.row_order` says so), **not** by `cluster_id` as in
  standard phy. Without that column, row k is the k-th unit in `cluster_info.tsv`.
