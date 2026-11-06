//! The nightly summary, shown as a sheet from the Sleep rows.

use super::{dots, duration, present_sheet, temperature_text, FAHRENHEIT};
use crate::store::{Metric, Night, Store};
use adw::prelude::*;
use chrono::{Local, TimeZone};

const NOTE: &str = "Estimated from movement, heart rate and signal confidence. Ringmaster is not a medical device.";

fn clock(unix: i64) -> String {
    Local.timestamp_opt(unix, 0).single().map(|t| t.format("%H:%M").to_string()).unwrap_or_default()
}

/// Dot class, title, value and subtitle of one row.
type Line = (&'static str, &'static str, String, String);

fn row(dot: &'static str, title: &'static str, value: &str, subtitle: &str) -> Line {
    (dot, title, value.to_string(), subtitle.to_string())
}

fn action_row((dot, title, value, subtitle): &Line) -> adw::ActionRow {
    let r = adw::ActionRow::builder().title(*title).subtitle(subtitle.as_str()).build();
    let d = dots::dot(dot);
    d.set_width_request(16);
    r.add_prefix(&d);
    r.add_suffix(&gtk::Label::builder().label(value.as_str()).valign(gtk::Align::Center).css_classes(["numeric", "title-4"]).build());
    r
}

fn minutes(seconds: i64) -> String {
    format!("{} min", seconds / 60)
}

/// Rows for the summary, in display order.
fn rows(n: &Night, fahrenheit: bool) -> Vec<Line> {
    let mut out = vec![row(
        "metric-sleep",
        "Asleep",
        &duration(n.asleep()),
        &format!("{}–{} · Awake {}", clock(n.start), clock(n.end), minutes(n.awake)),
    )];
    if let Some(hr) = n.heart_rate {
        let low = n.lowest_heart_rate.map(|l| format!("Lowest 15 minutes: {l:.0} bpm")).unwrap_or_default();
        out.push(row(dots::metric(Metric::HeartRate), "Heart rate", &format!("{hr:.0} bpm"), &low));
    }
    if let Some(hrv) = n.hrv {
        out.push(row(dots::metric(Metric::Hrv), "Heart rate variability", &format!("{hrv:.0} ms"), ""));
    }
    if let Some(spo2) = n.spo2 {
        let below = if n.below_90 > 0 { format!("Below 90 %: {}", minutes(n.below_90)) } else { "Never below 90 %".into() };
        let low = n.lowest_spo2.map(|l| format!("Lowest: {l:.0} % · {below}")).unwrap_or(below);
        out.push(row(dots::metric(Metric::Spo2), "Blood oxygen", &format!("{spo2:.0} %"), &low));
    }
    if let Some(resp) = n.resp_rate {
        out.push(row(dots::metric(Metric::RespRate), "Respiratory rate", &format!("{resp:.1} breaths/min"), ""));
    }
    if let Some(temp) = n.skin_temp {
        let scale = if fahrenheit { 9.0 / 5.0 } else { 1.0 };
        let unit = if fahrenheit { "°F" } else { "°C" };
        let usual = match n.usual_skin_temp {
            Some(u) => {
                let delta = (temp - u) * scale;
                match delta {
                    d if d.abs() < 0.05 => "Same as your usual".to_string(),
                    d if d > 0.0 => format!("{d:.1} {unit} above your usual"),
                    d => format!("{:.1} {unit} below your usual", -d),
                }
            }
            None => "Your usual needs 3 nights with skin temperature".into(),
        };
        out.push(row(dots::metric(Metric::SkinTemp), "Skin temperature", &temperature_text(temp as f32, fahrenheit), &usual));
    }
    out
}

pub fn present(from: &impl IsA<gtk::Widget>, store: &Store, ring: &str, start: i64, end: i64) {
    let night = match store.night(ring, start, end) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "reading the night failed");
            return;
        }
    };
    let day = Local.timestamp_opt(end, 0).single().map(|t| t.format("%A, %-d %B").to_string()).unwrap_or_default();
    let group = adw::PreferencesGroup::builder().title(day).description(NOTE).build();
    for line in rows(&night, FAHRENHEIT.get()) {
        group.add(&action_row(&line));
    }
    let page = adw::PreferencesPage::new();
    page.add(&group);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&page));
    let dialog = adw::Dialog::builder().title("Night").content_width(400).content_height(560).child(&view).build();
    present_sheet(&dialog, from);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temperature_against_the_usual() {
        let n = Night { start: 0, end: 8 * 3600, awake: 1200, skin_temp: Some(33.4), usual_skin_temp: Some(33.0), ..Default::default() };
        let r = rows(&n, true);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].2, "7 h 40 min");
        assert_eq!(r[0].3.split(" · ").nth(1), Some("Awake 20 min"));
        assert_eq!(r[1].3, "0.7 °F above your usual");
        let r = rows(&Night { spo2: Some(95.4), lowest_spo2: Some(91.0), ..n }, false);
        assert_eq!((r[1].2.as_str(), r[1].3.as_str()), ("95 %", "Lowest: 91 % · Never below 90 %"));
    }
}
