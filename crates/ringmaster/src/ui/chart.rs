//! Single-stream time chart drawn with cairo.

use crate::store::{Metric, Stats};
use chrono::{Duration as Days, Local, TimeZone};
use gtk::cairo;
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Default)]
struct Data {
    metric: Option<Metric>,
    from: i64,
    to: i64,
    points: Vec<(i64, f64)>,
    /// Estimated sleep, shaded behind the readings.
    sleep: Vec<(i64, i64)>,
    /// What the last draw plotted: (x, y, bucket midpoint, value), and the bucket width.
    drawn: Vec<(f64, f64, i64, f64)>,
    width: i64,
    /// Pointer or finger x position.
    hover: Option<f64>,
}

#[derive(Clone)]
pub struct Chart {
    pub area: gtk::DrawingArea,
    data: Rc<RefCell<Data>>,
}

const LEFT: f64 = 34.0;
const RIGHT: f64 = 8.0;
const TOP: f64 = 8.0;
const BOTTOM: f64 = 20.0;
/// Readings further apart than this are not joined.
const MIN_GAP: i64 = 10 * 60;
/// Matches `.metric-sleep` in style.css.
pub const SLEEP: (f64, f64, f64) = (0.369, 0.310, 0.839);

impl Chart {
    pub fn new(height: i32) -> Self {
        let area = gtk::DrawingArea::builder().content_height(height).hexpand(true).build();
        let data: Rc<RefCell<Data>> = Default::default();
        let d = data.clone();
        area.set_draw_func(move |area, cr, w, h| draw(area, cr, w as f64, h as f64, &mut d.borrow_mut()));
        let chart = Self { area, data };
        chart.track_pointer();
        chart
    }

    pub fn set(&self, metric: Metric, from: i64, to: i64, points: Vec<(i64, f64)>, sleep: Vec<(i64, i64)>) {
        self.data.replace(Data { metric: Some(metric), from, to, points, sleep, ..Default::default() });
        self.area.queue_draw();
    }

    fn hover(&self, x: Option<f64>) {
        self.data.borrow_mut().hover = x;
        self.area.queue_draw();
    }

    /// A mouse shows values on hover. A finger shows them on touch and while dragging sideways;
    /// a mostly vertical drag is released so the page still scrolls.
    fn track_pointer(&self) {
        let motion = gtk::EventControllerMotion::new();
        let c = self.clone();
        motion.connect_motion(move |_, x, _| c.hover(Some(x)));
        let c = self.clone();
        motion.connect_leave(move |_| c.hover(None));
        self.area.add_controller(motion);

        let drag = gtk::GestureDrag::builder().touch_only(true).build();
        let start = Rc::new(std::cell::Cell::new(0.0));
        let claimed = Rc::new(std::cell::Cell::new(false));
        let (c, s, k) = (self.clone(), start.clone(), claimed.clone());
        drag.connect_drag_begin(move |_, x, _| {
            s.set(x);
            k.set(false);
            c.hover(Some(x));
        });
        let (c, s, k) = (self.clone(), start, claimed);
        drag.connect_drag_update(move |g, dx, dy| {
            if !k.get() {
                if dy.abs() > 10.0 && dy.abs() > dx.abs() {
                    g.set_state(gtk::EventSequenceState::Denied);
                    c.hover(None);
                    return;
                }
                if dx.abs() > 8.0 {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    k.set(true);
                }
            }
            c.hover(Some(s.get() + dx));
        });
        let c = self.clone();
        drag.connect_drag_end(move |_, _, _| c.hover(None));
        let c = self.clone();
        drag.connect_cancel(move |_, _| c.hover(None));
        self.area.add_controller(drag);
    }
}

pub use crate::store::local_midnight;

/// Averages readings into buckets of `width` seconds; returns bucket midpoints.
fn bucket(points: &[(i64, f64)], from: i64, width: i64) -> Vec<(i64, f64)> {
    let mut out: Vec<(i64, f64)> = Vec::new();
    let (mut key, mut sum, mut n) = (i64::MIN, 0.0, 0);
    for &(t, v) in points {
        let k = (t - from).div_euclid(width);
        if k != key && n > 0 {
            out.push((from + key * width + width / 2, sum / n as f64));
            sum = 0.0;
            n = 0;
        }
        key = k;
        sum += v;
        n += 1;
    }
    if n > 0 {
        out.push((from + key * width + width / 2, sum / n as f64));
    }
    out
}

/// Bucket widths that divide a day, so buckets start on the clock when the chart starts at
/// midnight and hover times read as round values.
fn bucket_width(raw: f64) -> i64 {
    // 60 s keeps skin temperature's one reading a minute separate in the hour view.
    const WIDTHS: [i64; 11] = [60, 150, 300, 600, 900, 1800, 3600, 7200, 10_800, 21_600, 43_200];
    WIDTHS.into_iter().find(|w| *w as f64 >= raw).unwrap_or(86_400)
}

fn nice_step(range: f64) -> f64 {
    let raw = (range / 3.0).max(1e-9);
    let mag = 10f64.powf(raw.log10().floor());
    [1.0, 2.0, 5.0, 10.0].iter().map(|m| m * mag).find(|s| *s >= raw).unwrap_or(10.0 * mag)
}

fn time_ticks(from: i64, to: i64) -> Vec<(i64, String)> {
    let span = to - from;
    let start = Local.timestamp_opt(from, 0).single().map(|t| t.date_naive()).unwrap_or_default();
    if span <= 2 * 3600 {
        let step = 15 * 60;
        let mut t = from.div_euclid(step) * step + step;
        let mut ticks = Vec::new();
        while t <= to {
            if let Some(local) = Local.timestamp_opt(t, 0).single() {
                ticks.push((t, local.format("%H:%M").to_string()));
            }
            t += step;
        }
        return ticks;
    }
    if span <= 2 * 86_400 {
        let mut ticks = Vec::new();
        let mut day = start;
        loop {
            let midnight = local_midnight(day);
            if midnight >= to {
                break;
            }
            for hour in [0, 6, 12, 18] {
                let t = midnight + hour * 3600;
                if t >= from && t <= to {
                    ticks.push((t, format!("{hour:02}:00")));
                }
            }
            day += Days::days(1);
        }
        return ticks;
    }
    let days = (span / 86_400).max(1);
    if days <= 10 {
        let mut ticks = Vec::new();
        let mut day = start;
        while local_midnight(day) < to {
            let noon = local_midnight(day) + 12 * 3600;
            if noon > from {
                ticks.push((noon, day.format("%a").to_string()));
            }
            day += Days::days(1);
        }
        return ticks;
    }
    let every = (days as f64 / 6.0).ceil() as i64;
    let mut ticks = Vec::new();
    let mut day = start + Days::days(1);
    let mut i = 0;
    while local_midnight(day) <= to {
        if i % every == 0 {
            ticks.push((local_midnight(day), day.format("%-d %b").to_string()));
        }
        day += Days::days(1);
        i += 1;
    }
    ticks
}

fn draw(area: &gtk::DrawingArea, cr: &cairo::Context, w: f64, h: f64, data: &mut Data) {
    data.drawn.clear();
    let fg = area.color();
    let (r, g, b) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(10.0);
    let (plot_w, plot_h) = (w - LEFT - RIGHT, h - TOP - BOTTOM);
    let Some(metric) = data.metric else { return };
    if data.points.is_empty() || data.to <= data.from || plot_w <= 0.0 {
        cr.set_source_rgba(r, g, b, 0.5);
        let text = "No readings";
        if let Ok(ext) = cr.text_extents(text) {
            cr.move_to((w - ext.width()) / 2.0, h / 2.0);
            let _ = cr.show_text(text);
        }
        return;
    }

    let span = data.to - data.from;
    let width = bucket_width(span as f64 / plot_w);
    let points = bucket(&data.points, data.from, width);
    let stats = Stats::of(points.iter().map(|p| p.1)).unwrap();
    let step = nice_step((stats.max - stats.min).max(1.0));
    let (lo, hi) = ((stats.min / step).floor() * step, ((stats.max / step).ceil() * step).max((stats.min / step).floor() * step + step));
    let x = |t: i64| LEFT + (t - data.from) as f64 / span as f64 * plot_w;
    let y = |v: f64| TOP + (hi - v) / (hi - lo) * plot_h;

    let (sr, sg, sb) = SLEEP;
    cr.set_source_rgba(sr, sg, sb, 0.14);
    for &(start, end) in &data.sleep {
        let (x0, x1) = (x(start.max(data.from)), x(end.min(data.to)));
        cr.rectangle(x0, TOP, x1 - x0, plot_h);
        let _ = cr.fill();
    }
    cr.set_line_width(1.0);
    let mut v = lo;
    while v <= hi + step / 2.0 {
        cr.set_source_rgba(r, g, b, 0.12);
        cr.move_to(LEFT, y(v).round() + 0.5);
        cr.line_to(w - RIGHT, y(v).round() + 0.5);
        let _ = cr.stroke();
        cr.set_source_rgba(r, g, b, 0.6);
        let label = if step < 1.0 { format!("{v:.1}") } else { format!("{v:.0}") };
        if let Ok(ext) = cr.text_extents(&label) {
            cr.move_to(LEFT - 6.0 - ext.width(), y(v) + ext.height() / 2.0);
            let _ = cr.show_text(&label);
        }
        v += step;
    }
    {
        cr.set_source_rgba(r, g, b, 0.25);
        cr.set_dash(&[3.0, 3.0], 0.0);
        let mut day = Local.timestamp_opt(data.from, 0).single().map(|t| t.date_naive()).unwrap_or_default() + Days::days(1);
        while local_midnight(day) < data.to {
            let mx = x(local_midnight(day)).round() + 0.5;
            cr.move_to(mx, TOP);
            cr.line_to(mx, TOP + plot_h);
            let _ = cr.stroke();
            day += Days::days(1);
        }
        cr.set_dash(&[], 0.0);
    }
    cr.set_source_rgba(r, g, b, 0.6);
    for (t, label) in time_ticks(data.from, data.to) {
        if let Ok(ext) = cr.text_extents(&label) {
            let lx = (x(t) - ext.width() / 2.0).clamp(LEFT, w - RIGHT - ext.width());
            cr.move_to(lx, h - 5.0);
            let _ = cr.show_text(&label);
        }
    }

    data.drawn = points.iter().map(|&(t, v)| (x(t), y(v), t, v)).collect();
    data.width = width;
    let (_, (cr_r, cr_g, cr_b)) = metric.style();
    cr.set_source_rgb(cr_r, cr_g, cr_b);
    let gap = (3 * width).max(MIN_GAP);
    // Sparse streams read better as dots than as broken line fragments.
    let dots = metric == Metric::Spo2 || points.len() < 24;
    if dots {
        for &(t, v) in &points {
            cr.arc(x(t), y(v), 2.0, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
        draw_hover(cr, w, (r, g, b), data);
        return;
    }
    cr.set_line_width(1.5);
    cr.set_line_join(cairo::LineJoin::Round);
    let mut prev: Option<i64> = None;
    for &(t, v) in &points {
        match prev {
            Some(p) if t - p <= gap => cr.line_to(x(t), y(v)),
            _ => {
                let _ = cr.stroke();
                cr.move_to(x(t), y(v));
                cr.line_to(x(t) + 0.5, y(v));
            }
        }
        prev = Some(t);
    }
    let _ = cr.stroke();
    // Over a short span the line hides how few readings there are; each one gets a dot.
    if span <= 2 * 3600 {
        for &(t, v) in &points {
            cr.arc(x(t), y(v), 2.0, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
    }
    draw_hover(cr, w, (r, g, b), data);
}

/// Time span of a plotted bucket, for example "Tue 14:00–15:00" or "14:30".
fn hover_time(t: i64, width: i64, span: i64) -> String {
    let fmt = |u: i64| Local.timestamp_opt(u, 0).single().map(|t| t.format("%H:%M").to_string()).unwrap_or_default();
    let day = if span > 86_400 {
        Local.timestamp_opt(t, 0).single().map(|t| t.format("%a ").to_string()).unwrap_or_default()
    } else {
        String::new()
    };
    // Readings arrive every 150 s; a wider bucket is an average over its span.
    if width > 300 {
        format!("{day}{}–{}", fmt(t - width / 2), fmt(t + width / 2))
    } else {
        format!("{day}{}", fmt(t))
    }
}

fn draw_hover(cr: &cairo::Context, w: f64, (r, g, b): (f64, f64, f64), data: &Data) {
    let (Some(hx), Some(metric)) = (data.hover, data.metric) else { return };
    let Some(&(px, py, t, v)) = data.drawn.iter().min_by(|a, b| (a.0 - hx).abs().total_cmp(&(b.0 - hx).abs())) else { return };
    let (_, (mr, mg, mb)) = metric.style();
    cr.set_source_rgba(r, g, b, 0.4);
    cr.set_line_width(1.0);
    cr.move_to(px.round() + 0.5, TOP);
    cr.line_to(px.round() + 0.5, py);
    let _ = cr.stroke();
    cr.set_source_rgb(mr, mg, mb);
    cr.arc(px, py, 4.0, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();

    let label = format!("{} · {:.*} {}", hover_time(t, data.width, data.to - data.from), metric.precision(), v, super::metric_unit(metric));
    cr.set_font_size(11.0);
    let Ok(ext) = cr.text_extents(&label) else { return };
    let (pad, bw, bh) = (5.0, ext.width() + 10.0, ext.height() + 10.0);
    let bx = (px - bw / 2.0).clamp(LEFT, (w - RIGHT - bw).max(LEFT));
    // The label sits above the line unless the point is near the top.
    let by = if py - TOP > bh + 6.0 { TOP } else { (py + 8.0).min(TOP + 60.0) };
    cr.set_source_rgba(r, g, b, 0.9);
    rounded(cr, bx, by, bw, bh, 4.0);
    let _ = cr.fill();
    // The foreground is light on a dark theme and dark on a light one.
    let light = (r + g + b) / 3.0 > 0.5;
    let text = if light { 0.0 } else { 1.0 };
    cr.set_source_rgb(text, text, text);
    cr.move_to(bx + pad - ext.x_bearing(), by + pad - ext.y_bearing());
    let _ = cr.show_text(&label);
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_average_within_each_window() {
        let points = [(0, 60.0), (100, 62.0), (400, 70.0)];
        assert_eq!(bucket(&points, 0, 300), vec![(150, 61.0), (450, 70.0)]);
    }

    #[test]
    fn bucket_widths_divide_a_day() {
        assert_eq!(bucket_width(100.0), 150);
        assert_eq!(bucket_width(9.0), 60);
        assert_eq!(bucket_width(4000.0), 7200);
        assert!([60, 150, 300, 600, 900, 1800, 3600, 7200, 10_800, 21_600, 43_200].iter().all(|w| 86_400 % w == 0));
    }

    #[test]
    fn hour_ticks_fall_on_quarter_hours() {
        let from = 1_761_590_000;
        let ticks = time_ticks(from, from + 3600);
        assert_eq!(ticks.len(), 4);
        assert!(ticks.iter().all(|(t, _)| t % 900 == 0));
    }

    #[test]
    fn steps_are_round() {
        assert_eq!(nice_step(40.0), 20.0);
        assert_eq!(nice_step(9.0), 5.0);
        assert_eq!(nice_step(2.0), 1.0);
    }
}
