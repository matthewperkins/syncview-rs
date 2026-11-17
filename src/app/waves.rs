//! The spike loupe and the waveform grid.
//!
//! Holding Alt (Option) over a row shows the loupe: a few milliseconds around the pointer at full
//! sample rate, spikes in their units' colours, the nearest spike's unit template drawn over it.
//! A click while it shows pins the nearest spike in its own wire's panel of the waveform grid; a
//! right click pins that moment on every wire (a neuron is local, pickup shows everywhere). The
//! grid draws pinned snippets over their units' templates (± SD), on one shared scale, and can
//! overlay every spike of the selected unit in the current view.

use super::*;

/// The loupe's half width (s).
const LOUPE_HALF_S: f64 = 0.003;
/// At most this many pins; the oldest goes first.
const MAX_PINS: usize = 12;
/// At most this many spikes in the "all spikes in view" overlay (evenly spread over the view).
const MAX_OVERLAY: usize = 400;
/// (no blues: the default trace colour and tab10's first unit colour are blue)
const PIN_COLORS: [[u8; 3]; 8] = [
    [0xf0, 0xe4, 0x42], [0xff, 0xff, 0xff], [0xff, 0x6e, 0xc7], [0xe6, 0x9f, 0x00],
    [0x4c, 0xe0, 0x8a], [0xd5, 0x5e, 0x00], [0xc0, 0xa0, 0xff], [0x9a, 0xd0, 0x8a],
];

/// What the loupe is over this frame.
pub(super) struct Loupe {
    row: usize,
    t: f64,
    pos: Pos2,
}

/// A pinned spike (or moment): snippets of the wires it was pinned on, cut when pinned.
pub(super) struct Pin {
    unit: Option<i64>,
    wire: String,
    t: f64,
    all: bool,
    color: Color32,
    data: Vec<(String, Vec<f32>)>,
}

impl App {
    /// Samples before and after a spike in snippets: the templates' window, else ±1.5 ms.
    fn window_samples(&self) -> (usize, usize) {
        match self.spikes.as_ref().and_then(|s| s.templates.as_ref()) {
            Some(t) => (t.n_before, t.n_samples - t.n_before),
            None => {
                let h = (0.0015 * self.rec.fs).round() as usize;
                (h, h)
            }
        }
    }

    /// The unit spike nearest `t` (within `within` s) on a row's wire: (unit id, spike time).
    fn nearest_spike(&self, row: usize, t: f64, within: f64) -> Option<(i64, f64)> {
        let sp = self.spikes.as_ref()?;
        if self.is_car_itself(row) {
            return None;
        }
        let fs = self.rec.fs;
        let ch = &self.rows[row].spec.ch;
        let (a, b) = (((t - within) * fs).floor() as i64, ((t + within) * fs).ceil() as i64 + 1);
        sp.units
            .iter()
            .filter(|u| u.wire == *ch && !self.hidden_units.contains(&u.id))
            .flat_map(|u| u.times[u.range(a, b)].iter().map(move |&s| (u.id, s as f64 / fs)))
            .min_by(|x, y| (x.1 - t).abs().total_cmp(&(y.1 - t).abs()))
    }

    pub(super) fn is_car_itself(&self, row: usize) -> bool {
        self.rows[row].spec.reference.as_deref().and_then(crate::refs::CarDef::parse).is_some_and(|d| d.variant == crate::refs::Variant::Only)
    }

    /// The row a wire's snippets come from: the shown row of that channel, preferring one with
    /// the same reference as `like` (so a pin compares like with like).
    fn row_for_wire(&self, wire: &str, like: Option<&str>) -> Option<usize> {
        let rows: Vec<usize> = (0..self.rows.len()).filter(|&i| self.rows[i].spec.ch == wire && !self.is_car_itself(i)).collect();
        let same = |i: &&usize| {
            let r = self.rows[**i].spec.reference.as_deref();
            match (r, like) {
                (Some(a), Some(b)) => a.split(':').next() == b.split(':').next(),
                (None, None) => true,
                _ => false,
            }
        };
        rows.iter().find(same).or(rows.first()).copied()
    }

    fn snippet(&self, row: usize, t: f64) -> Option<Vec<f32>> {
        let (before, after) = self.window_samples();
        let tr = self.trace_for(&self.rows[row])?;
        if (tr.dt * self.rec.fs - 1.0).abs() > 1e-6 {
            return None; // not a full-rate trace
        }
        tr.snippet(t, before, after)
    }

    /// The pointer and keys over the chart (end of traces_ui): the loupe, and pinning with it.
    /// Returns true while Alt is held (the chart then doesn't pan on drags).
    pub(super) fn loupe_input(&mut self, ctx: &egui::Context, hover: Option<Pos2>, hover_row: Option<usize>, (x0, plot_w): (f32, f32)) -> bool {
        let alt = ctx.input(|i| i.modifiers.alt);
        self.loupe = None;
        let (Some(pos), Some(row), true) = (hover, hover_row, alt) else { return alt };
        let t = self.t - self.span / 2.0 + ((pos.x - x0) / plot_w) as f64 * self.span;
        self.loupe = Some(Loupe { row, t, pos });
        let (left, right) = ctx.input(|i| (i.pointer.button_clicked(egui::PointerButton::Primary), i.pointer.button_clicked(egui::PointerButton::Secondary)));
        if left || right {
            self.pin(row, t, right);
        }
        alt
    }

    fn pin(&mut self, row: usize, t: f64, all: bool) {
        let near = self.nearest_spike(row, t, LOUPE_HALF_S);
        if near.is_none() && !all {
            self.status = "no sorted spike under the loupe (right-click pins the moment on every wire)".into();
            return;
        }
        let (unit, at) = near.map_or((None, t), |(u, s)| (Some(u), s));
        let wire = self.rows[row].spec.ch.clone();
        let like = self.rows[row].spec.reference.clone();
        let mut data = vec![];
        if let Some(y) = self.snippet(row, at) {
            data.push((wire.clone(), y));
        }
        if all {
            for w in self.grid_wires() {
                if w != wire
                    && let Some(r) = self.row_for_wire(&w, like.as_deref())
                    && let Some(y) = self.snippet(r, at)
                {
                    data.push((w, y));
                }
            }
        }
        if data.is_empty() {
            self.status = "the trace there isn't at full sample rate yet (wait for filtering, or zoom in)".into();
            return;
        }
        let c = PIN_COLORS[self.pin_seq % PIN_COLORS.len()];
        self.pin_seq += 1;
        self.pins.push(Pin { unit, wire, t: at, all, color: Color32::from_rgb(c[0], c[1], c[2]), data });
        if self.pins.len() > MAX_PINS {
            self.pins.remove(0);
        }
        self.waves_open = true;
    }

    /// The wires the grid shows: the export's brain wires (that the recording has), else the
    /// shown rows' channels.
    fn grid_wires(&self) -> Vec<String> {
        match &self.spikes {
            Some(sp) => sp.brain.iter().filter(|w| self.rec.ch(w).is_some()).cloned().collect(),
            None => {
                let mut v: Vec<String> = vec![];
                for r in &self.rows {
                    if !v.contains(&r.spec.ch) {
                        v.push(r.spec.ch.clone());
                    }
                }
                v
            }
        }
    }

    /// The loupe, drawn beside the pointer.
    pub(super) fn loupe_ui(&self, ctx: &egui::Context) {
        let Some(l) = &self.loupe else { return };
        let fs = self.rec.fs;
        let half = (LOUPE_HALF_S * fs).round() as usize;
        let row = &self.rows[l.row];
        let (w, h) = (300.0f32, 170.0f32);
        let screen = ctx.content_rect();
        let mut at = l.pos + Vec2::new(18.0, 18.0);
        if at.x + w > screen.right() {
            at.x = l.pos.x - 18.0 - w;
        }
        if at.y + h > screen.bottom() {
            at.y = l.pos.y - 18.0 - h;
        }
        egui::Area::new(egui::Id::new("loupe")).order(egui::Order::Tooltip).fixed_pos(at).interactable(false).show(ctx, |ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
            let p = ui.painter_at(rect);
            p.rect_filled(rect, 4.0, Color32::from_rgb(0x14, 0x16, 0x1c));
            p.rect_stroke(rect, 4.0, Stroke::new(1.0, MUTED), egui::StrokeKind::Inside);
            let plot = rect.shrink2(Vec2::new(8.0, 20.0));
            let near = self.nearest_spike(l.row, l.t, LOUPE_HALF_S);
            let title = match near {
                Some((u, s)) => format!("{}  ·  u{u} at {:.4} s", row.spec.title(), s),
                None => format!("{}  ·  {:.4} s", row.spec.title(), l.t),
            };
            p.text(rect.left_top() + Vec2::new(8.0, 4.0), Align2::LEFT_TOP, title, FontId::proportional(11.0), LABEL);
            p.text(rect.left_bottom() + Vec2::new(8.0, -4.0), Align2::LEFT_BOTTOM, "click: pin   right-click: pin on every wire", FontId::proportional(9.5), MUTED);
            let Some(tr) = self.trace_for(row).filter(|tr| (tr.dt * fs - 1.0).abs() < 1e-6) else {
                p.text(plot.center(), Align2::CENTER_CENTER, "not at full sample rate here yet", FontId::proportional(11.0), MUTED);
                return;
            };
            let Some(y) = tr.snippet(l.t, half, half) else { return };
            let tmpl = near.and_then(|(u, s)| {
                let sp = self.spikes.as_ref()?;
                let unit = sp.units.iter().find(|x| x.id == u)?;
                let (m, _) = sp.templates.as_ref()?.get(unit, &row.spec.ch)?;
                Some((s, m, sp.templates.as_ref()?.n_before, unit_color(unit.color)))
            });
            let peak = y.iter().chain(tmpl.iter().flat_map(|t| t.1.iter())).fold(1.0f32, |a, v| a.max(v.abs())) * 1.1;
            let n = y.len();
            let sx = |k: f64| plot.left() + (k / (n - 1) as f64) as f32 * plot.width();
            let sy = |v: f32| plot.center().y - v / peak * plot.height() / 2.0;
            p.line_segment([Pos2::new(plot.left(), plot.center().y), Pos2::new(plot.right(), plot.center().y)], Stroke::new(1.0, Color32::from_white_alpha(20)));
            p.line_segment([Pos2::new(plot.center().x, plot.top()), Pos2::new(plot.center().x, plot.bottom())], Stroke::new(1.0, Color32::from_white_alpha(30)));
            if let Some((s, m, before, c)) = &tmpl {
                let k0 = (s - l.t) * fs + half as f64 - *before as f64;
                let pts: Vec<Pos2> = m.iter().enumerate().map(|(k, v)| Pos2::new(sx(k0 + k as f64), sy(*v))).collect();
                p.add(egui::Shape::line(pts, Stroke::new(3.0, c.lerp_to_gamma(Color32::WHITE, 0.3).gamma_multiply(0.55))));
            }
            p.add(egui::Shape::line(y.iter().enumerate().map(|(k, v)| Pos2::new(sx(k as f64), sy(*v))).collect(), Stroke::new(1.2, row.color)));
            // spikes in the loupe, in their colours
            if let Some(sp) = &self.spikes
                && !self.is_car_itself(l.row)
            {
                let first = (l.t * fs).round() as i64 - half as i64;
                let hw = (SPIKE_HALF_S * fs) as i64;
                for u in sp.units.iter().filter(|u| u.wire == row.spec.ch && !self.hidden_units.contains(&u.id)) {
                    for &s in &u.times[u.range(first, first + n as i64)] {
                        let (a, b) = (((s - hw - first).max(0)) as usize, ((s + hw - first).min(n as i64 - 1)) as usize);
                        if b > a {
                            p.add(egui::Shape::line((a..=b).map(|k| Pos2::new(sx(k as f64), sy(y[k]))).collect(), Stroke::new(1.8, on_trace(unit_color(u.color), row.color))));
                        }
                    }
                }
            }
            p.text(plot.right_top(), Align2::RIGHT_TOP, format!("±{peak:.0} µV, ±{:.0} ms", LOUPE_HALF_S * 1000.0), FontId::proportional(9.5), MUTED);
        });
    }

    /// The waveform grid window.
    pub(super) fn waves_ui(&mut self, ctx: &egui::Context) {
        if !self.waves_open {
            return;
        }
        let mut open = true;
        let mut remove = None;
        let mut clear = false;
        egui::Window::new("Waveforms").open(&mut open).resizable(true).default_size([560.0, 460.0]).show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                let sel = self.selected_unit;
                ui.add_enabled(sel.is_some(), egui::Checkbox::new(&mut self.waves_all, format!("all spikes of {} in view", sel.map_or("the selected unit".into(), |u| format!("u{u}")))))
                    .on_hover_text("every spike of the unit selected in the Units tab within the chart's view, faintly, over its template")
                    .on_disabled_hover_text("select a unit in the Units tab");
                if ui.add_enabled(!self.pins.is_empty(), egui::Button::new("Clear")).clicked() {
                    clear = true;
                }
                for (k, pin) in self.pins.iter().enumerate() {
                    let text = format!("{} {} {:.4} s{}", pin.unit.map_or("·".into(), |u| format!("u{u}")), pin.wire, pin.t, if pin.all { " (all wires)" } else { "" });
                    if ui.add(egui::Button::new(egui::RichText::new(text).color(pin.color).small())).on_hover_text("click to remove this pin").clicked() {
                        remove = Some(k);
                    }
                }
            });
            if self.pins.is_empty() && !self.waves_all {
                ui.label(egui::RichText::new("Hold Alt (Option on a Mac) over a row for the loupe; click to pin the spike under it here, right-click to pin that moment on every wire.").small().color(MUTED));
            }
            let area = ui.available_rect_before_wrap();
            ui.allocate_rect(area, Sense::hover());
            self.draw_grid(ui, area);
        });
        self.waves_open = open;
        if clear {
            self.pins.clear();
        }
        if let Some(k) = remove {
            self.pins.remove(k);
        }
    }

    /// The "all spikes in view" overlay: snippets of the selected unit's spikes in the chart's view.
    fn overlay(&self) -> Option<(String, Vec<Vec<f32>>, Color32)> {
        let sp = self.spikes.as_ref()?;
        let u = sp.units.iter().find(|u| Some(u.id) == self.selected_unit)?;
        let row = self.row_for_wire(&u.wire, None)?;
        let tr = self.trace_for(&self.rows[row])?;
        if (tr.dt * self.rec.fs - 1.0).abs() > 1e-6 {
            return None;
        }
        let fs = self.rec.fs;
        let (before, after) = self.window_samples();
        let (lo, hi) = (self.t - self.span / 2.0, self.t + self.span / 2.0);
        let r = u.range((lo * fs).floor() as i64, (hi * fs).ceil() as i64);
        let step = (r.len() / MAX_OVERLAY).max(1);
        let snips = u.times[r].iter().step_by(step).filter_map(|&s| tr.snippet(s as f64 / fs, before, after)).collect();
        Some((u.wire.clone(), snips, unit_color(u.color)))
    }

    fn draw_grid(&self, ui: &egui::Ui, area: Rect) {
        let wires = self.grid_wires();
        if wires.is_empty() || area.width() < 60.0 || area.height() < 60.0 {
            return;
        }
        let p = ui.painter_at(area);
        let sp = self.spikes.as_ref();
        let templates = sp.and_then(|s| s.templates.as_ref());
        let unit = |id: i64| sp.and_then(|s| s.units.iter().find(|u| u.id == id));
        let overlay = if self.waves_all { self.overlay() } else { None };
        // one scale for every panel, so amplitudes compare across wires
        let mut peak = 1.0f32;
        for pin in &self.pins {
            for (_, y) in &pin.data {
                peak = y.iter().fold(peak, |a, v| a.max(v.abs()));
            }
        }
        if let Some((_, snips, _)) = &overlay {
            for y in snips {
                peak = y.iter().fold(peak, |a, v| a.max(v.abs()));
            }
        }
        let peak = peak * 1.1;
        let cols = (wires.len() as f32).sqrt().ceil() as usize;
        let rows = wires.len().div_ceil(cols);
        let (pw, ph) = (area.width() / cols as f32, (area.height() - 14.0) / rows as f32);
        p.text(area.right_bottom(), Align2::RIGHT_BOTTOM, format!("±{peak:.0} µV per panel"), FontId::proportional(10.0), MUTED);
        for (k, w) in wires.iter().enumerate() {
            let cell = Rect::from_min_size(area.min + Vec2::new((k % cols) as f32 * pw, (k / cols) as f32 * ph), Vec2::new(pw, ph)).shrink(3.0);
            p.rect_filled(cell, 3.0, Color32::from_rgb(0x16, 0x18, 0x1f));
            let plot = cell.shrink2(Vec2::new(4.0, 14.0));
            p.text(cell.left_top() + Vec2::new(4.0, 2.0), Align2::LEFT_TOP, w, FontId::proportional(10.0), LABEL);
            p.line_segment([Pos2::new(plot.left(), plot.center().y), Pos2::new(plot.right(), plot.center().y)], Stroke::new(1.0, Color32::from_white_alpha(18)));
            let sx = |k: usize, n: usize| plot.left() + k as f32 / (n.max(2) - 1) as f32 * plot.width();
            let sy = |v: f32| plot.center().y - (v / peak).clamp(-1.2, 1.2) * plot.height() / 2.0;
            let line = |y: &[f32], stroke: Stroke| egui::Shape::line(y.iter().enumerate().map(|(k, v)| Pos2::new(sx(k, y.len()), sy(*v))).collect(), stroke);
            // templates: each pinned unit's on this wire (± SD on its own wire)
            let mut shown = vec![];
            let ids = self.pins.iter().filter_map(|pin| pin.unit.map(|u| (u, pin.all || pin.wire == *w))).chain(overlay.iter().filter(|o| o.0 == *w).filter_map(|_| self.selected_unit.map(|u| (u, true))));
            for (id, here) in ids {
                if !here || shown.contains(&id) {
                    continue;
                }
                shown.push(id);
                let (Some(t), Some(u)) = (templates, unit(id)) else { continue };
                let Some((m, sd)) = t.get(u, w) else { continue };
                let c = unit_color(u.color);
                if let Some(sd) = sd.filter(|_| u.wire == *w) {
                    let n = m.len();
                    let mut band: Vec<Pos2> = (0..n).map(|k| Pos2::new(sx(k, n), sy(m[k] + sd[k]))).collect();
                    band.extend((0..n).rev().map(|k| Pos2::new(sx(k, n), sy(m[k] - sd[k]))));
                    // (a strip of quads: the band isn't convex)
                    for k in 0..n - 1 {
                        p.add(egui::Shape::convex_polygon(vec![band[k], band[k + 1], band[2 * n - 2 - k], band[2 * n - 1 - k]], c.gamma_multiply(0.18), Stroke::NONE));
                    }
                }
                p.add(line(&m, Stroke::new(2.5, c.gamma_multiply(if u.wire == *w { 0.8 } else { 0.4 }))));
            }
            if let Some((ow, snips, c)) = &overlay
                && ow == w
            {
                for y in snips {
                    p.add(line(y, Stroke::new(1.0, c.gamma_multiply(0.18))));
                }
            }
            for pin in &self.pins {
                if let Some((_, y)) = pin.data.iter().find(|(pw, _)| pw == w) {
                    p.add(line(y, Stroke::new(if pin.wire == *w { 1.6 } else { 1.1 }, pin.color)));
                }
            }
            if self.pins.iter().all(|pin| pin.data.iter().all(|(pw, _)| pw != w)) && self.row_for_wire(w, None).is_none() {
                p.text(plot.center(), Align2::CENTER_CENTER, "no row shows it", FontId::proportional(9.5), MUTED);
            }
        }
    }
}
