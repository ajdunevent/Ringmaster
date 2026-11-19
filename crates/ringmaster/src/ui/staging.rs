//! Sleep stages on screen: the stage chart for a night, the strip in each row of the Nights list,
//! and the rows that give each stage's time against the wearer's usual.

use crate::stages::{Minutes, Night, Stage};
use adw::prelude::*;
use chrono::{Local, TimeZone};
use gtk::cairo;
use std::rc::Rc;

/// Stages in the order rows and legends list them.
pub const SHOWN: [Stage; 4] = [Stage::Deep, Stage::Rem, Stage::Light, Stage::Awake];

/// Within this many minutes a stage counts as about the usual.
const ABOUT_USUAL: f64 = 15.0;

/// Matches the `.stage-*` classes in style.css.
pub fn colour(s: Stage) -> (f64, f64, f64) {
    match s {
        Stage::Deep => (0.369, 0.310, 0.839),
        Stage::Rem => (0.110, 0.620, 0.561),
        Stage::Light => (0.722, 0.686, 0.941),
        Stage::Awake => (0.898, 0.647, 0.039),
    }
}

pub fn class(s: Stage) -> &'static str {
    match s {
        Stage::Deep => "stage-deep",
        Stage::Rem => "stage-rem",
        Stage::Light => "stage-light",
        Stage::Awake => "stage-awake",
    }
}

pub fn name(s: Stage) -> &'static str {
    match s {
        Stage::Deep => "Deep",
        Stage::Rem => "REM",
        Stage::Light => "Light",
        Stage::Awake => "Awake",
    }
}

/// "1h 08m" from minutes.
pub fn minutes(m: f64) -> String {
    super::duration((m * 60.0).round() as i64)
}

/// "more than your usual 1h 08m", or "about your usual (1h 08m)".
pub fn against_usual(night: &Night, s: Stage) -> String {
    let Some(usual) = night.usual else { return String::new() };
    let (now, then) = (night.minutes.of(s), usual.of(s));
    if (now - then).abs() < ABOUT_USUAL {
        format!("about your usual ({})", minutes(then))
    } else {
        format!("{} than your usual {}", if now > then { "more" } else { "less" }, minutes(then))
    }
}

/// "Deep 2h 20m · REM 1h 12m".
pub fn deep_and_rem(m: &Minutes) -> String {
    format!("Deep {} · REM {}", minutes(m.of(Stage::Deep)), minutes(m.of(Stage::Rem)))
}

/// The note for a night that never settled, when it was one.
pub fn unsettled_text(night: &Night) -> Option<String> {
    let (above, below) = night.unsettled?;
    let what = match (above, below) {
        (Some(a), Some(b)) => format!("heart rate stayed {a:.0} bpm above your usual and HRV {b:.0} ms below it"),
        (Some(a), None) => format!("heart rate stayed {a:.0} bpm above your usual"),
        (None, Some(b)) => format!("HRV stayed {b:.0} ms below your usual"),
        (None, None) => return None,
    };
    Some(format!("<b>Restless night:</b> {what}, so little of the night looked like deep sleep."))
}

/// A row per stage, with its time and how it compares with the usual.
pub fn rows(night: &Night) -> Vec<adw::PreferencesRow> {
    SHOWN
        .iter()
        .map(|s| {
            let row = adw::ActionRow::builder().title(name(*s)).subtitle(against_usual(night, *s)).build();
            row.add_prefix(&super::dots::dot(class(*s)));
            row.add_suffix(
                &gtk::Label::builder().label(minutes(night.minutes.of(*s))).valign(gtk::Align::Center).css_classes(["numeric"]).build(),
            );
            row.upcast()
        })
        .collect()
}

/// The Sleep row of a whole day: time asleep, the times, and a strip of the night's stages.
pub fn strip_row(value: &str, subtitle: &str, m: &Minutes) -> adw::PreferencesRow {
    let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(3).hexpand(true).valign(gtk::Align::Center).build();
    text.append(&gtk::Label::builder().label("Sleep").xalign(0.0).build());
    text.append(&gtk::Label::builder().label(subtitle).xalign(0.0).wrap(true).css_classes(["caption", "dim-label"]).build());
    let strip = strip(m);
    strip.set_margin_top(3);
    text.append(&strip);
    let content = gtk::Box::builder().spacing(12).margin_top(8).margin_bottom(8).margin_start(12).margin_end(12).build();
    content.append(&super::dots::dot("metric-sleep"));
    content.append(&text);
    content.append(&gtk::Label::builder().label(value).valign(gtk::Align::Center).css_classes(["numeric"]).build());
    adw::PreferencesRow::builder().activatable(false).child(&content).title("Sleep").build()
}

pub fn unsettled_row(text: &str) -> adw::PreferencesRow {
    let label = gtk::Label::builder().label(text).use_markup(true).wrap(true).xalign(0.0).hexpand(true).build();
    let icon = gtk::Image::builder().icon_name("dialog-warning-symbolic").css_classes(["warning"]).valign(gtk::Align::Start).build();
    let content = gtk::Box::builder().spacing(12).margin_top(10).margin_bottom(10).margin_start(12).margin_end(12).build();
    content.append(&icon);
    content.append(&label);
    adw::PreferencesRow::builder().activatable(false).child(&content).build()
}

/// The stage chart: a lane per stage, from the night's first sleep to its last.
pub fn chart_row(night: &Night) -> adw::PreferencesRow {
    let area = gtk::DrawingArea::builder().content_height(108).hexpand(true).build();
    let data: Rc<Vec<(i64, Stage)>> = Rc::new(night.sleeps.iter().flat_map(|s| s.stages.iter().copied()).collect());
    let span = (night.sleeps.first().map(|s| s.start), night.sleeps.last().map(|s| s.end));
    let (start, end) = (span.0.unwrap_or(0), span.1.unwrap_or(1));
    {
        let data = data.clone();
        area.set_draw_func(move |area, cr, w, h| draw(area, cr, w as f64, h as f64, &data, start, end));
    }
    let at = |u: i64| Local.timestamp_opt(u, 0).single().map(|t| t.format("%H:%M").to_string()).unwrap_or_default();
    let axis = gtk::Box::builder().margin_start(LABELS as i32).build();
    axis.append(&gtk::Label::builder().label(at(start)).css_classes(["caption", "dim-label"]).hexpand(true).xalign(0.0).build());
    axis.append(&gtk::Label::builder().label(at(end)).css_classes(["caption", "dim-label"]).xalign(1.0).build());
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(12)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&area);
    content.append(&axis);
    area.update_property(&[gtk::accessible::Property::Label(&format!(
        "Sleep stages from {} to {}: {}",
        at(start),
        at(end),
        SHOWN.iter().map(|s| format!("{} {}", name(*s).to_lowercase(), minutes(night.minutes.of(*s)))).collect::<Vec<_>>().join(", ")
    ))]);
    adw::PreferencesRow::builder().activatable(false).child(&content).build()
}

/// Width of the stage names left of the lanes.
const LABELS: f64 = 46.0;
/// Lanes from the top.
const LANES: [Stage; 4] = [Stage::Awake, Stage::Rem, Stage::Light, Stage::Deep];

fn draw(area: &gtk::DrawingArea, cr: &cairo::Context, w: f64, h: f64, data: &[(i64, Stage)], start: i64, end: i64) {
    let fg = area.color();
    let (r, g, b) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(10.0);
    let lane = h / LANES.len() as f64;
    let plot = w - LABELS;
    for (i, s) in LANES.iter().enumerate() {
        let y = i as f64 * lane;
        cr.set_source_rgba(r, g, b, 0.6);
        if let Ok(ext) = cr.text_extents(name(*s)) {
            cr.move_to(0.0, y + (lane + ext.height()) / 2.0);
            let _ = cr.show_text(name(*s));
        }
        if i > 0 {
            cr.set_source_rgba(r, g, b, 0.08);
            cr.set_line_width(1.0);
            cr.move_to(LABELS, y.round() + 0.5);
            cr.line_to(w, y.round() + 0.5);
            let _ = cr.stroke();
        }
    }
    let span = (end - start).max(1) as f64;
    let x = |t: i64| LABELS + (t - start) as f64 / span * plot;
    let step = crate::sleep::EPOCH;
    let mut i = 0;
    while i < data.len() {
        // A run of one stage, while its epochs follow each other.
        let (from, stage) = data[i];
        let mut to = from + step;
        let mut j = i + 1;
        while j < data.len() && data[j].1 == stage && data[j].0 - to <= step {
            to = data[j].0 + step;
            j += 1;
        }
        let lane_index = LANES.iter().position(|l| *l == stage).unwrap_or(0);
        let (cr_, cg, cb) = colour(stage);
        cr.set_source_rgb(cr_, cg, cb);
        let top = lane_index as f64 * lane + lane * 0.18;
        let width = (x(to) - x(from)).max(1.5);
        rounded(cr, x(from), top, width, lane * 0.64, 2.0);
        let _ = cr.fill();
        i = j;
    }
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

/// A thin bar split by the share of each stage.
pub fn strip(m: &Minutes) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder().content_height(6).hexpand(true).build();
    let m = *m;
    area.set_draw_func(move |_, cr, w, h| {
        let total: f64 = SHOWN.iter().map(|s| m.of(*s)).sum();
        if total <= 0.0 {
            return;
        }
        rounded(cr, 0.0, 0.0, w as f64, h as f64, 3.0);
        cr.clip();
        let mut x = 0.0;
        for s in SHOWN {
            let width = m.of(s) / total * w as f64;
            let (r, g, b) = colour(s);
            cr.set_source_rgb(r, g, b);
            cr.rectangle(x, 0.0, (width - 1.0).max(0.0), h as f64);
            let _ = cr.fill();
            x += width;
        }
    });
    area
}

/// Coloured dots naming the stages in `stages`, centred under a chart.
pub fn legend(stages: &[Stage]) -> gtk::Box {
    let b = gtk::Box::builder().spacing(14).halign(gtk::Align::Center).css_classes(["caption"]).build();
    for s in stages {
        b.append(&super::dots::labeled(class(*s), name(*s)));
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn night(minutes: [f64; 4], usual: Option<[f64; 4]>) -> Night {
        Night { minutes: Minutes(minutes), usual: usual.map(Minutes), ..Default::default() }
    }

    #[test]
    fn stages_against_the_usual() {
        let n = night([25.0, 250.0, 140.0, 72.5], Some([12.5, 245.0, 67.5, 60.0]));
        assert_eq!(against_usual(&n, Stage::Deep), "more than your usual 1h 08m");
        assert_eq!(against_usual(&n, Stage::Rem), "about your usual (1h 00m)");
        assert_eq!(against_usual(&night([0.0; 4], None), Stage::Deep), "");
        assert_eq!(deep_and_rem(&n.minutes), "Deep 2h 20m · REM 1h 13m");
    }

    #[test]
    fn restless_note() {
        let mut n = night([0.0; 4], None);
        assert_eq!(unsettled_text(&n), None);
        n.unsettled = Some((Some(8.0), Some(11.0)));
        assert_eq!(
            unsettled_text(&n).unwrap(),
            "<b>Restless night:</b> heart rate stayed 8 bpm above your usual and HRV 11 ms below it, so little of the night looked like deep sleep."
        );
        n.unsettled = Some((None, Some(6.0)));
        assert!(unsettled_text(&n).unwrap().contains("HRV stayed 6 ms below your usual"));
    }
}
