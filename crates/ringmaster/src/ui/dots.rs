//! Coloured dots that tie every label, selector and chart to its kind of data.

use crate::settings::Stat;
use crate::store::{MeasureKind, Metric};
use gtk::prelude::*;

pub const TEMPERATURE: &str = "metric-temp";
pub const STEPS: &str = "metric-steps";
pub const DEVICE: &str = "metric-device";

pub fn metric(m: Metric) -> &'static str {
    m.style().0
}

pub fn measure(kind: MeasureKind) -> &'static str {
    match kind {
        MeasureKind::HeartRate => metric(Metric::HeartRate),
        MeasureKind::Spo2 => metric(Metric::Spo2),
    }
}

pub fn stat(s: Stat) -> &'static str {
    match s {
        Stat::Battery | Stat::CaseBattery | Stat::DaysSinceSync => DEVICE,
        Stat::SkinTemp => TEMPERATURE,
        Stat::HeartRate => metric(Metric::HeartRate),
        Stat::Hrv => metric(Metric::Hrv),
        Stat::Spo2 => metric(Metric::Spo2),
        Stat::RespRate => metric(Metric::RespRate),
        Stat::DailySteps | Stat::StepsToGoal => STEPS,
    }
}

pub fn dot(class: &str) -> gtk::Label {
    gtk::Label::builder().label("●").valign(gtk::Align::Center).css_classes(["dot", class]).build()
}

pub fn labeled(class: &str, text: &str) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    b.append(&dot(class));
    b.append(&gtk::Label::builder().label(text).xalign(0.0).build());
    b
}

/// A drop-down whose items and selection each show their dot.
pub fn dropdown(items: Vec<(&'static str, String)>) -> gtk::DropDown {
    let labels: Vec<&str> = items.iter().map(|(_, l)| l.as_str()).collect();
    let model = gtk::StringList::new(&labels);
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.append(&gtk::Label::builder().label("●").valign(gtk::Align::Center).css_classes(["dot"]).build());
        row.append(&gtk::Label::builder().xalign(0.0).build());
        item.set_child(Some(&row));
    });
    let classes: Vec<&'static str> = items.iter().map(|(c, _)| *c).collect();
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let Some(row) = item.child().and_downcast::<gtk::Box>() else { return };
        let (Some(dot), Some(label)) = (row.first_child().and_downcast::<gtk::Label>(), row.last_child().and_downcast::<gtk::Label>())
        else {
            return;
        };
        let position = item.position() as usize;
        dot.set_css_classes(&["dot", classes.get(position).copied().unwrap_or(DEVICE)]);
        if let Some(text) = item.item().and_downcast::<gtk::StringObject>() {
            label.set_text(&text.string());
        }
    });
    gtk::DropDown::builder().model(&model).factory(&factory).build()
}
