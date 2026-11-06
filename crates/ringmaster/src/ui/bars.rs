//! A bar per day with an optional goal line, for the Sleep and Activity pages.

use gtk::cairo;
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

const LEFT: f64 = 34.0;
const RIGHT: f64 = 6.0;
const TOP: f64 = 10.0;
const BOTTOM: f64 = 20.0;

pub struct Bar {
    /// Under the bar; only every few are drawn when bars are narrow.
    pub label: String,
    pub value: f64,
}

#[derive(Default)]
struct Data {
    bars: Vec<Bar>,
    goal: Option<f64>,
    colour: (f64, f64, f64),
    /// Axis and goal labels.
    format: Option<Box<dyn Fn(f64) -> String>>,
}

#[derive(Clone)]
pub struct Bars {
    pub area: gtk::DrawingArea,
    data: Rc<RefCell<Data>>,
}

impl Bars {
    pub fn new(height: i32) -> Self {
        let area = gtk::DrawingArea::builder().content_height(height).hexpand(true).build();
        let data: Rc<RefCell<Data>> = Default::default();
        let d = data.clone();
        area.set_draw_func(move |area, cr, w, h| draw(area, cr, w as f64, h as f64, &d.borrow()));
        Self { area, data }
    }

    pub fn set(&self, bars: Vec<Bar>, goal: Option<f64>, colour: (f64, f64, f64), format: impl Fn(f64) -> String + 'static) {
        self.data.replace(Data { bars, goal, colour, format: Some(Box::new(format)) });
        self.area.queue_draw();
    }
}

/// A round step giving about four gridlines below `top`.
fn step(top: f64) -> f64 {
    let raw = top / 4.0;
    let magnitude = 10f64.powf(raw.log10().floor());
    [1.0, 2.0, 2.5, 5.0, 10.0].iter().map(|m| m * magnitude).find(|s| *s >= raw).unwrap_or(raw)
}

fn draw(area: &gtk::DrawingArea, cr: &cairo::Context, w: f64, h: f64, data: &Data) {
    let fg = area.color();
    let (r, g, b) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(10.0);
    let (plot_w, plot_h) = (w - LEFT - RIGHT, h - TOP - BOTTOM);
    let format = |v: f64| data.format.as_ref().map_or_else(|| format!("{v}"), |f| f(v));
    if data.bars.iter().all(|b| b.value <= 0.0) || plot_w <= 0.0 {
        cr.set_source_rgba(r, g, b, 0.5);
        let text = "Nothing recorded yet";
        if let Ok(ext) = cr.text_extents(text) {
            cr.move_to((w - ext.width()) / 2.0, h / 2.0);
            let _ = cr.show_text(text);
        }
        return;
    }
    let top = data.bars.iter().map(|b| b.value).fold(data.goal.unwrap_or(0.0), f64::max) * 1.05;
    let step = step(top);
    let top = (top / step).ceil() * step;
    let y = |v: f64| TOP + (1.0 - v / top) * plot_h;

    cr.set_line_width(1.0);
    let mut v = 0.0;
    while v <= top + step / 2.0 {
        cr.set_source_rgba(r, g, b, 0.12);
        cr.move_to(LEFT, y(v).round() + 0.5);
        cr.line_to(w - RIGHT, y(v).round() + 0.5);
        let _ = cr.stroke();
        cr.set_source_rgba(r, g, b, 0.6);
        let label = format(v);
        if let Ok(ext) = cr.text_extents(&label) {
            cr.move_to(LEFT - 4.0 - ext.width(), y(v) + ext.height() / 2.0);
            let _ = cr.show_text(&label);
        }
        v += step;
    }

    let n = data.bars.len() as f64;
    let slot = plot_w / n;
    let bar = (slot * 0.7).max(1.0);
    let (cr_, cg, cb) = data.colour;
    for (i, item) in data.bars.iter().enumerate() {
        let x = LEFT + i as f64 * slot + (slot - bar) / 2.0;
        if item.value > 0.0 {
            let reached = data.goal.is_none_or(|g| item.value >= g);
            cr.set_source_rgba(cr_, cg, cb, if reached { 1.0 } else { 0.55 });
            let radius = (bar / 2.0).min(3.0);
            rounded_top(cr, x, y(item.value), bar, y(0.0) - y(item.value), radius);
            let _ = cr.fill();
        }
    }
    // Labels spaced to stay at least 24 px apart, counted back from the newest bar.
    let every = (24.0 / slot).ceil().max(1.0) as usize;
    cr.set_source_rgba(r, g, b, 0.6);
    for (i, item) in data.bars.iter().enumerate().rev().step_by(every) {
        let Ok(ext) = cr.text_extents(&item.label) else { continue };
        cr.move_to(LEFT + i as f64 * slot + (slot - ext.width()) / 2.0, h - 6.0);
        let _ = cr.show_text(&item.label);
    }

    if let Some(goal) = data.goal {
        cr.set_source_rgba(r, g, b, 0.7);
        cr.set_dash(&[4.0, 3.0], 0.0);
        cr.move_to(LEFT, y(goal).round() + 0.5);
        cr.line_to(w - RIGHT, y(goal).round() + 0.5);
        let _ = cr.stroke();
        cr.set_dash(&[], 0.0);
    }
}

fn rounded_top(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let r = r.min(h);
    cr.new_sub_path();
    cr.move_to(x, y + h);
    cr.line_to(x, y + r);
    cr.arc(x + r, y + r, r, PI, 3.0 * FRAC_PI_2);
    cr.arc(x + w - r, y + r, r, 3.0 * FRAC_PI_2, 0.0);
    cr.line_to(x + w, y + h);
    cr.close_path();
}

#[cfg(test)]
mod tests {
    use super::step;

    #[test]
    fn round_steps() {
        assert_eq!(step(9.0), 2.5);
        assert_eq!(step(12_000.0), 5_000.0);
        assert_eq!(step(4.0), 1.0);
    }
}
