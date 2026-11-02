<img src="assets/icon.png" alt="syncview icon: an engraved mouse head with its headstage and optical ferrule, above a smoked-drum heart trace" width="128" align="right">

# syncview-rs

**Watch Open Ephys recordings and behaviour video together, frame by frame, and mark what the
animal does.** Each video frame is locked to its camera trigger, so the traces under the cursor are
what was happening in the frame on screen.

![syncview on macOS: video frame-locked to masseter and digastric EMG and antrum and duodenum slow
waves, with the channel table and a whole-session slow-wave power overview](docs/screenshot-macos.png)

*syncview on macOS: 4 of 16 channels of a 10-minute excerpt (masseter and digastric EMG, antrum and
duodenum slow waves) with the antrum's slow-wave power across the excerpt (bottom; the shaded box is
the current view).*

- **Fast:** filters a 2-hour, 16-channel session once (about 5 s), then scrolls and zooms at the
  display's refresh rate.
- **Up to four cameras**, each locked to its own TTL line.
- **Annotation:** mark bites, bouts, contractions… from the keyboard; saved as CSV whose sample
  indexes line up with spike-sorter output.
- **Layout:** video above the chart, or either floating over the other; per-row trace clipping.
- **Clip export:** video with the traces scrolling underneath, as MP4.
- macOS, Windows and Linux; MIT-licensed (`LICENSE`).

## Quick start

1. **Install**
   - **macOS (Apple silicon):** in Terminal,
     ```bash
     cd ~/Downloads && curl -L -O https://github.com/matthewperkins/syncview-rs/releases/latest/download/syncview-macos-arm64.zip \
       && unzip -o syncview-macos-arm64.zip && mv syncview.app /Applications/
     ```
   - **Windows (x64):** unzip `syncview-windows-x64.zip` from the
     [latest release](https://github.com/matthewperkins/syncview-rs/releases/latest) and run `syncview.exe`
     (first time: *More info* → *Run anyway*).
   - **Linux:** `cargo install --git https://github.com/matthewperkins/syncview-rs --features static-ffmpeg`,
     then `syncview --install-desktop` (details in [Install](#install)).
2. **Open a session:** drag its folder (recording, videos and preset JSON together) onto the window,
   or run `syncview /path/to/session`.
3. **No data at hand?** Click **Try the demo** on the start page.

## Cheat sheet

| To | Keys / mouse |
|---|---|
| Step one video frame | ← / → (Shift: 10 % of the view) |
| Play / pause, speed | Space, `[` / `]` |
| Zoom time | scroll, or `+` / `-` |
| Pan time | drag, sideways swipe, or Shift+scroll |
| Page, start, end | PgUp / PgDn, Home / End; click or drag the overview strip to jump |
| Scale one row / reset it | ⌘ or Ctrl + scroll over the row / double-click it |
| Hide the sidebar | F2 |
| Mark an event | its label's key at the start, again at the end (labels shown under the toolbar) |
| Mark with the other hand | Shift + key primes a label; the key left of `1` (`` ` ``) stamps it |
| Cancel an open event / undo / redo | Esc / Ctrl+Z / Ctrl+Shift+Z (Cmd on macOS) |
| Go through events | ↑ / ↓ in the Events tab; Ctrl/Cmd + ← / → by time; Delete removes the selected one |

## Contents

[Install](#install) · [Try it without a rig](#try-it-without-a-rig) · [Opening recordings](#opening-recordings) ·
[The viewer](#the-viewer) · [Annotating events](#annotating-events) · [Export a clip](#export-a-clip) ·
[How it draws](#how-it-draws) · [Checked](#checked) · [About](#about)

## Install

**Prebuilt (macOS, Apple silicon).** In Terminal:

```bash
cd ~/Downloads
curl -L -O https://github.com/matthewperkins/syncview-rs/releases/latest/download/syncview-macos-arm64.zip
unzip -o syncview-macos-arm64.zip
mv syncview.app /Applications/
```

Then open syncview from Launchpad, Spotlight or the Applications folder (drag it to the Dock to keep
it there). The command-line program is inside the app; to type `syncview` in Terminal, link it
onto your PATH once:

```bash
sudo mkdir -p /usr/local/bin && sudo ln -sf /Applications/syncview.app/Contents/MacOS/syncview /usr/local/bin/syncview
syncview --demo
```

The app is not signed with an Apple Developer ID. A file downloaded with `curl` isn't marked as
quarantined, so macOS opens it straight away. If you download the zip with a browser instead,
macOS refuses the first launch: clear the flag once with
`xattr -dr com.apple.quarantine /Applications/syncview.app`, or use System Settings → Privacy &
Security → Open Anyway.

**Prebuilt (Windows, x64).** Download `syncview-windows-x64.zip` from the
[latest release](https://github.com/matthewperkins/syncview-rs/releases/latest), unzip it and
double-click `syncview.exe` (right-click → *Pin to Start* to keep it in the Start menu), or run it
from PowerShell (`.\syncview.exe --demo`). The build is not code-signed, so the first time Windows
shows "Windows protected your PC": click *More info* → *Run anyway*. Linux and Intel Macs build
from source as below.

Both prebuilt binaries are built by GitHub Actions from the tagged commit
(`.github/workflows/release.yml`).

**From source.** You need Rust (1.89 or newer) and a C compiler. The `static-ffmpeg` option compiles
FFmpeg into syncview, so nothing else is needed and the program keeps working when system packages
change.

```bash
# macOS (once): Apple's compiler tools and Rust
xcode-select --install
brew install rust            # or https://rustup.rs

# then
cargo install --git https://github.com/matthewperkins/syncview-rs --features static-ffmpeg
```

That builds and installs the `syncview` command into `~/.cargo/bin` (add it to your PATH if your
shell doesn't find it). The first build takes about 5–10 minutes, most of it compiling FFmpeg. To
update, run the same command again.

On Linux the same command works; x86-64 also needs `nasm` (e.g. `sudo pacman -S nasm`). Then
`syncview --install-desktop` adds it to your application launcher, with its icon (it writes
`~/.local/share/applications/syncview.desktop`; run it again if the program moves).

**Building against the system's FFmpeg instead** (faster first build, but it has to be rebuilt
whenever the system's FFmpeg changes major version): install FFmpeg's libraries and `pkgconf`
(`brew install ffmpeg pkgconf`, or `sudo pacman -S ffmpeg`) and leave out `--features static-ffmpeg`.
From a checkout: `cargo build --release` gives `target/release/syncview`.

## Try it without a rig

Click **Try the demo** on the start page, or run `syncview --demo`. Either writes a synthetic
five-minute recording, a matching cartoon video and a preset (~100 MB, about 10 s, first run only)
into the cache folder, then opens them.

It contains a chewing jaw (masseter and digastric EMG, a jaw-position sensor and a bipolar masseter
pair whose reference wire carries only hum and movement artefact), antral slow waves that grow after
each meal, duodenal slow waves, and mains hum for the notch. Each video frame shows its frame number
and its trigger's time, so the sync can be checked by eye. `syncview --demo DIR` writes it into DIR
instead. The folder is an ordinary Open Ephys recording, so Python syncview opens it too (see its
`README.txt`).

## Opening recordings

```bash
syncview                          # start page: drop a folder and/or a video, or try the demo
syncview /path/to/folder          # finds the recording, videos and preset inside it
```

### The start page

- **Drag an Open Ephys folder** onto the window, or click *Choose folder…*. It can be a whole day's
  session, a Record Node or one recording.
- **Several recordings:** a list opens; Up / Down and Enter pick one. Each shows its length,
  channels, start time and camera triggers.
- **Several videos:** a second list asks which go with the recording. Enter opens the highlighted
  one (or none); Space or the box beside each video ticks several cameras (up to 4).
- **One video** can be dropped with the folder, or before it (it's shown with a preview frame).
- **Sync check:** each chosen video's frames are compared with its camera triggers before the viewer
  opens (in the background, with a progress bar per video: the first time, this is where its
  frames are indexed). If they differ by more than 2, a warning says so: open anyway, open
  without that video, or go back.
- **While viewing,** drop a video on the window to add it, or another folder to go back to the start
  page with it.
- **The first open of a recording or video** does work that is then remembered: filtering the whole
  recording into the cache, and indexing each video's frames (an MP4 / MOV without reordered
  frames from its own frame tables; any other video by reading it once). A banner under the
  toolbar shows each step's progress; the traces on screen are filtered on the fly meanwhile.

### From the command line

A folder works as on the start page: syncview searches it for Open Ephys recordings (folders
containing `structure.oebin`, or `.nwb` files), videos (`.mp4`, `.mov`, `.avi`, `.mkv`) and a
preset JSON at its top level (if there are several, the one named `preset.json`). With one recording
and at most one video it opens them straight away; otherwise the start page asks. In Terminal you
can type `syncview ` and drag the folder in from Finder. To skip the search, name the files:

```bash
syncview --rec "/path/to/Record Node 101/experiment1/recording1" \
          --video /path/to/BASLER_CAM_….mp4 \
          --preset presets.json          # optional; default: all electrode channels as EMG
```

| option | default | |
|---|---|---|
| `FOLDER` | | a folder to search for the recording, videos and preset (above) |
| `--rec FOLDER` | none (start page) | Open Ephys recording folder (contains `structure.oebin`) or `.nwb` file |
| `--video FILE` | none | video recorded during this recording; repeat for several cameras (up to 4) |
| `--preset FILE.json` | built from the recording | same JSON as Python syncview |
| `--stream NAME` | `acquisition_board`, else the first stream | Open Ephys continuous stream |
| `--trigger-line N` | `1` | TTL line with one pulse per video frame |
| `--cache FOLDER` | `$SYNCVIEW_RS_CACHE`, else `~/.cache/syncview-rs` (Linux), `~/Library/Caches/syncview-rs` (macOS) | filtered traces |
| `--time S`, `--time-base S`, `--play` | | initial position, view width, start playing |
| `--install-desktop` | | Linux: add syncview to the application launcher |

## The viewer

The [cheat sheet](#cheat-sheet) lists the keys. The sidebar on the right has two tabs, **Channels**
and **Events**; F2 hides it to a thin rail so the video and chart get the full width.

### Channel table

One row per trace: Show, Label, Ch, Ref (bipolar reference), Mode, Low/High Hz (blank = no filter
edge), Notch (e.g. `60` or `60,180`), DSP wzrd (`order=3 smooth_ms=10 env_lp=40 plot_fs=1000
win_s=30`), Y range (`auto` or `lo, hi`), and Clip.

- **Clip** is a three-position switch: a trace that leaves its Y range is cut off at its own row, at
  the chart's edges, or nowhere (it can then draw over the video and the overview strip); it keeps
  its row's scale either way.
- Text fields apply on Enter or when you click away; input that doesn't parse reverts.
- Changing a row's mode resets its band and DSP wzrd settings to that mode's defaults (Python
  syncview keeps the old band).
- Click a row's number to select it for Up / Down / Remove, and as the template for Add.
- **Load…/Save…** read and write presets in the Python format (time base, rows, overview), plus
  syncview-rs's own clip settings, layout and annotation labels, which the Python version ignores.
- Rows named alike apart from a trailing number (masseter1 … masseter4, antrum 1 … antrum 4) share a
  colour; a preset's own `color` wins.

### Several cameras

Up to four videos play together, each locked to its own TTL line (line 1, or `--trigger-line`,
unless changed). **Videos** in the toolbar lists them, with each one's TTL line, Remove and Add
video…. The first video sets the frame number, frame stepping (← / →), annotation frames and clip
export.

### Layout

By default the videos sit side by side above the chart. **Float video** turns each video into a box
over the chart, **Float chart** turns the chart into a box over the video, and with both on each is
a box of its own; the chart is always drawn on top. While the padlock is open, drag a box to move it
and its bottom-right corner to resize it; lock it and drags pan time again (the wheel zooms either
way). **dim** darkens the video where the chart covers it, and **outline** draws a dark edge under
every trace so it stands out over the video.

### Sharing a cache with Python syncview

Cache entries use the same keys and layout, so `--cache` pointed at a Python syncview cache folder
reuses its filtered traces and video indexes (and vice versa, untested in that direction since
v0.1.6: syncview-rs stores traces as float16, half the disk space of Python's float32). Slow-mode
traces are the exception: syncview-rs decimates before filtering them, so it keeps its own.

## Annotating events

Events are moments (points) or stretches (intervals) with a label, marked on the first video's
frames and saved next to the recording.

### Labels

The strip under the toolbar lists the labels with their keys. The defaults are B bite, C chewing
bout, O oro-manual phase, G grooming bout, D drinking, S sniffing and X gastric episode; a preset's
`labels` replace them:

```json
"labels": [
  {"name": "contraction", "key": "U", "kind": "interval", "color": "#e8876a"},
  {"name": "pup delivered", "key": "P", "kind": "point"}
]
```

### Marking

- **Step to the frame** with ← / → and press a label's key: a point is marked at once; an interval
  starts (its chip lights up) and the next press ends it. One interval per label can be open, so
  bites can be marked inside an open chewing bout.
- **One hand on the video, the other marking:** Shift + a label's key (or a click on its chip)
  primes it; the **stamp** key, the one left of `1` (`` ` `` on US keyboards, found by position),
  then starts and ends it. While playing, a stamp lands on the frame on screen.
- **Esc** cancels the last open interval; with none open, it unprimes. **Ctrl/Cmd+Z** undoes,
  **Ctrl/Cmd+Shift+Z** redoes.
- Events show as bands (points as ticks) on the chart and as bars on the overview strip.

### Reviewing

- The **Events** tab lists them; click a row, or use ↑ / ↓, to select one and go there.
  Ctrl/Cmd + ← / → jumps to the previous / next event in time from the cursor. A filter shows one
  label only.
- The selected event can get a note, have its start or end moved to the current frame
  (**Start = here**, **End = here**), or be removed (Delete). Every change can be undone.
- Fill in **annotator** in the Events tab; each event records who marked it.

### Audit mode

Tick **audit: video only** in the strip to score from the video alone: the traces and earlier marks
are hidden, and events marked then are saved with `view` = `video`. Once a label has been scored
both ways, the Events tab shows how the passes agree: events in each, matched pairs (overlapping, or
starting within 100 ms) and the mean onset and offset differences over the audited stretch.

### Files

- **`syncview_annotations.csv`** in the recording folder, saved a second after each change: one row
  per event, with times in seconds and sample indexes from the recording's first sample (as
  Kilosort counts), so they line up with sorted spikes. Comment lines at the top record the version,
  sample rate, Open Ephys first sample number and labels (`pandas.read_csv(path, comment="#")`).
- **`syncview_overlaps.csv`** beside it, rewritten on every save: every pair of overlapping events,
  same-label pairs first (usually double marks).
- Saves are atomic, a file that can't be read is never overwritten, and columns from a newer version
  are kept.

## Export a clip

**Export clip…** (in the toolbar, once a video is synced) saves an MP4 like Python syncview's
clips: the first video on top with a `t = … s   frame N` clock, and the rows shown underneath, with
their colours and Y ranges, scrolling past a fixed cursor at each frame's trigger.

| field | |
|---|---|
| From, To | the stretch of the recording (h:mm:ss or seconds); *Use the current view* fills them in |
| Time base | seconds of data across the chart, half before and half after each frame |
| Width | output width in pixels (blank: the video's own); the height follows |
| Speed | 1 = real time, 0.25 = four times slower (the frame rate is the camera's × speed) |

Export runs in the background, with progress and Cancel. From the command line, for batches:

```bash
syncview /path/to/folder --export-clip chewing.mp4 --clip-from 1:45 --clip-to 1:55 \
          [--clip-time-base 4] [--clip-width 1280] [--clip-speed 0.25]
```

This uses the preset's shown rows (Y ranges from the preset, or automatic). Clips are H.264 where
syncview's FFmpeg can encode it (macOS: Apple's VideoToolbox; builds against a system FFmpeg with
x264), otherwise FFmpeg's own MPEG-4 (Part 2) encoder at high quality (the prebuilt Windows binary).
VLC plays both; most other players and PowerPoint support MPEG-4 Part 2 too, but H.264 is the safer
choice for slides.

## How it draws

Each row is one GPU polyline (`src/gpu.rs`), drawn inside egui's render pass through an
`egui_wgpu` paint callback. As in syncview, a row never has more points than pixel columns: the
whole session is filtered once into a min/max pyramid on disk (`src/cache.rs`), and each frame reads
the coarsest level with at least one block per pixel column. Each segment is drawn as a quad with
antialiased sides, one logical pixel wide, so lines stay continuous on 1x screens too
(`SYNCVIEW_RS_LINE_PX` sets another width, in physical pixels).

## Checked

- **Filters vs scipy/Python:** `cargo test` compares every mode (plus notch, odd order and
  high-pass-only variants) with Python syncview's output on 200 s of synthetic data: max relative
  difference 10⁻⁹ (slow mode) to 10⁻¹⁵. Run `tests/make_fixtures.py` with a Python that has
  syncview first to generate the references (55 MB, not in git). Cache keys match Python's exactly.
- **Whole-session cache:** against Python's cache for a 10-minute excerpt: same lengths and pyramid
  depth, identical statistics, differences at float32 rounding. Built 17 traces in 5 s (Python ≈ 33 s).
  Since v0.1.6 traces are stored as float16: on a 2.2 h recording the cache shrinks from 3.3 to
  1.7 GB, values differ from float32 by 0.02% (median; at most ~2 µV on spikes of thousands of µV),
  and 160 of 1.7 million screenshot pixels change.
- **Video:** frames at 14 indexes (incl. both sides of keyframes and the last frame) are the same
  frames Python's decoder returns (mean grey difference 0.005 vs ≥ 0.33 between neighbours).
- **GUI:** Linux (Hyprland/Wayland, Vulkan, NVIDIA T400), 1648×983, 16 rows + video playing at 1×:
  58–60 frames/s (the display's refresh rate). macOS (Apple silicon, Metal, Retina; Homebrew Rust,
  FFmpeg and pkgconf): builds and runs with the 10-minute excerpt; trackpad and keys behave well.
- **Annotation:** marking, priming and stamping, review, undo and audit mode were driven with real
  key presses and clicks (xdotool on a private X display); the file round trip, overlaps and pass
  agreement have unit tests.
- **Not yet checked:** Windows by hand (CI builds it, runs the demo and takes screenshots),
  multi-hour recordings with video.

## About

A Rust port of the Python [syncview](https://github.com/matthewperkins/syncview): everything in it
is ported (the viewer, the channel editor and clip export), and it reads the same recordings and
presets, plus NWB recordings. Built with [egui](https://github.com/emilk/egui)/eframe for the
interface, [wgpu](https://wgpu.rs) (Vulkan, Metal, DirectX) for drawing the traces, and FFmpeg
(`ffmpeg-next`) for video. This repository was called syncviewR before v0.2, and the command
`syncviewr`.

### Provenance

Written by **Claude** (Anthropic), model **Claude Opus 5.5** (`claude-opus-5-5`, training cutoff
June 2026), in Claude Code from 2026-10-07, directed by **Matthew Perkins**, who holds the
copyright. It is a port of the Python syncview written the same way; the invariants listed in that
README's Provenance section (sync rule, frame indexing by packet-timestamp rank, cache keys) apply
here too and are what the tests above check.
