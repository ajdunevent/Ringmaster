//! Browse history: month calendar, the selected day's summary, and charts over an editable range.

use super::calendar::{first_of_month, mark_classes, MonthCalendar};
use super::chart::{local_midnight, Chart};
use super::dots;
use crate::store::{resting_heart_rate, DayMarks, Metric, Stats, Store};
use adw::prelude::*;
use chrono::{Datelike, Duration as Days, Local, NaiveDate, TimeZone};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

struct DatePick {
    button: gtk::MenuButton,
    calendar: gtk::Calendar,
    popover: gtk::Popover,
}

fn date_pick() -> DatePick {
    let calendar = gtk::Calendar::new();
    let popover = gtk::Popover::builder().child(&calendar).build();
    let button = gtk::MenuButton::builder().popover(&popover).valign(gtk::Align::Center).build();
    DatePick { button, calendar, popover }
}

fn range_row(from: &DatePick, to: &DatePick) -> adw::PreferencesRow {
    let line = gtk::Box::builder().spacing(8).halign(gtk::Align::Center).margin_top(8).margin_bottom(8).build();
    for (title, pick) in [("From", from), ("To", to)] {
        line.append(&gtk::Label::builder().label(title).css_classes(["dim-label"]).margin_start(if title == "To" { 4 } else { 0 }).build());
        line.append(&pick.button);
    }
    adw::PreferencesRow::builder().child(&line).activatable(false).build()
}

/// GLib's `%x` follows the locale's date format; chrono's does not.
fn show_date(row: &DatePick, day: NaiveDate) {
    let Ok(dt) = glib::DateTime::from_local(day.year(), day.month() as i32, day.day() as i32, 0, 0, 0.0) else { return };
    let label = dt.format("%x").map(|s| s.to_string()).unwrap_or_else(|_| day.format("%Y-%m-%d").to_string());
    row.button.set_label(&label);
    row.calendar.select_day(&dt);
}

fn calendar_date(cal: &gtk::Calendar) -> Option<NaiveDate> {
    let d = cal.date();
    NaiveDate::from_ymd_opt(d.year(), d.month() as u32, d.day_of_month() as u32)
}

struct Inner {
    store: Option<Rc<Store>>,
    ring: Rc<RefCell<Option<String>>>,
    calendar: MonthCalendar,
    day_group: adw::PreferencesGroup,
    day_rows: RefCell<Vec<adw::ActionRow>>,
    part: Cell<DayPart>,
    from_row: DatePick,
    to_row: DatePick,
    from: Cell<NaiveDate>,
    to: Cell<NaiveDate>,
    streams: RefCell<HashSet<Metric>>,
    charts: gtk::Box,
    sleep_button: gtk::ToggleButton,
    full_button: gtk::ToggleButton,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DayPart {
    Full,
    Awake,
    Asleep,
}

#[derive(Clone)]
pub struct HistoryPage {
    pub content: gtk::ScrolledWindow,
    inner: Rc<Inner>,
}

fn row(dot: &str, title: &str, value: &str, subtitle: &str) -> adw::ActionRow {
    let r = adw::ActionRow::builder().title(title).subtitle(subtitle).build();
    r.add_prefix(&dots::dot(dot));
    r.add_suffix(&gtk::Label::builder().label(value).valign(gtk::Align::Center).css_classes(["numeric"]).build());
    r
}

/// Seconds between SpO2 readings overnight; each reading stands for that long.
const SPO2_STEP: i64 = 300;

/// What the Sleep view adds to a row: time with blood oxygen below 90 %, when there was any, and
/// skin temperature against the usual of recent nights.
fn sleep_detail(store: &Store, ring: &str, metric: Metric, points: &[(i64, f64)], start: i64, mean: f64) -> String {
    match metric {
        Metric::Spo2 => {
            let below = points.iter().filter(|p| p.1 < 90.0).count() as i64 * SPO2_STEP;
            if below > 0 {
                format!(" · Below 90 %: {} min", below / 60)
            } else {
                String::new()
            }
        }
        Metric::SkinTemp => match store.usual_skin_temp(ring, start).ok().flatten() {
            Some(usual) => {
                let (scale, unit) = if super::FAHRENHEIT.get() { (9.0 / 5.0, "°F") } else { (1.0, "°C") };
                match (mean - usual) * scale {
                    d if d.abs() < 0.05 => " · As usual".into(),
                    d if d > 0.0 => format!(" · {d:.1} {unit} above usual"),
                    d => format!(" · {:.1} {unit} below usual", -d),
                }
            }
            None => String::new(),
        },
        _ => String::new(),
    }
}

fn legend() -> gtk::FlowBox {
    let flow = gtk::FlowBox::builder().selection_mode(gtk::SelectionMode::None).max_children_per_line(3).column_spacing(12).build();
    let marks = DayMarks { metrics: Metric::ALL.to_vec(), workout: true };
    let names = Metric::ALL
        .map(|m| match m {
            Metric::RespRate => "Resp rate",
            Metric::SkinTemp => "Skin temp",
            _ => m.short_name(),
        })
        .into_iter()
        .chain(["Workout"]);
    for (class, name) in mark_classes(&marks).into_iter().zip(names) {
        let item = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        item.append(&gtk::Label::builder().label("●").css_classes(["calendar-dot", class]).build());
        item.append(&gtk::Label::builder().label(name).css_classes(["caption"]).build());
        flow.insert(&item, -1);
    }
    flow
}

impl HistoryPage {
    pub fn new(store: Option<Rc<Store>>, ring: Rc<RefCell<Option<String>>>) -> Self {
        let calendar = MonthCalendar::new();
        let day_group = adw::PreferencesGroup::new();
        let average = gtk::Label::builder().label("Average").css_classes(["caption", "dim-label"]).valign(gtk::Align::End).build();
        day_group.set_header_suffix(Some(&average));
        let parts = gtk::Box::builder().css_classes(["linked"]).halign(gtk::Align::Center).margin_top(8).margin_bottom(8).build();
        let part_buttons: Vec<(gtk::ToggleButton, DayPart)> =
            [("Full day", DayPart::Full), ("Awake", DayPart::Awake), ("Sleep", DayPart::Asleep)]
                .into_iter()
                .map(|(label, part)| (gtk::ToggleButton::builder().label(label).active(part == DayPart::Full).build(), part))
                .collect();
        for (b, _) in &part_buttons {
            if b != &part_buttons[0].0 {
                b.set_group(Some(&part_buttons[0].0));
            }
            parts.append(b);
        }
        day_group.add(&adw::PreferencesRow::builder().child(&parts).activatable(false).build());

        // Linked like the stream buttons on the main page: one row, split after SpO2 into two
        // when the row does not fit.
        let toggle_rows = [
            gtk::Box::builder().css_classes(["linked"]).halign(gtk::Align::Center).build(),
            gtk::Box::builder().css_classes(["linked"]).halign(gtk::Align::Center).build(),
        ];
        let toggle_row = gtk::Box::builder().halign(gtk::Align::Center).css_classes(["joined"]).build();
        for r in &toggle_rows {
            toggle_row.append(r);
        }
        // Without the clip the row's one-line width would hold the window open, and it would
        // never stack. The empty area beside it reports the width there is.
        let toggle_clip = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::External)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_width(true)
            .child(&toggle_row)
            .build();
        let width_probe = gtk::DrawingArea::builder().hexpand(true).build();
        let toggles = gtk::Box::builder().orientation(gtk::Orientation::Vertical).margin_top(6).margin_bottom(6).build();
        toggles.append(&toggle_clip);
        toggles.append(&width_probe);
        super::stack_to_fit(&toggle_row, &width_probe);
        let from_row = date_pick();
        let to_row = date_pick();
        let controls = adw::PreferencesGroup::builder().title("Charts").build();
        controls.add(&toggles);
        controls.add(&range_row(&from_row, &to_row));
        let charts = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(18).build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        content.append(&calendar.widget);
        content.append(&legend());
        content.append(&day_group);
        content.append(&controls);
        content.append(&charts);
        let clamp = adw::Clamp::builder().maximum_size(600).child(&content).build();
        let content = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamp).vexpand(true).build();

        let today = Local::now().date_naive();
        let inner = Rc::new(Inner {
            store,
            ring,
            calendar: calendar.clone(),
            day_group,
            day_rows: Default::default(),
            part: Cell::new(DayPart::Full),
            from_row,
            to_row,
            from: Cell::new(today),
            to: Cell::new(today),
            streams: RefCell::new(Metric::ALL.into_iter().collect()),
            charts,
            sleep_button: part_buttons[2].0.clone(),
            full_button: part_buttons[0].0.clone(),
        });
        let this = Self { content, inner };
        for (button, part) in part_buttons {
            let this = this.clone();
            button.connect_toggled(move |b| {
                if b.is_active() {
                    this.inner.part.set(part);
                    this.load_day(this.inner.calendar.selected());
                }
            });
        }

        for metric in Metric::ALL {
            let label = dots::labeled(dots::metric(metric), metric.short_name());
            let toggle = gtk::ToggleButton::builder().child(&label).active(true).tooltip_text(metric.name()).build();
            let this = this.clone();
            toggle.connect_toggled(move |t| {
                {
                    let mut streams = this.inner.streams.borrow_mut();
                    if t.is_active() {
                        streams.insert(metric);
                    } else {
                        streams.remove(&metric);
                    }
                }
                this.load_charts();
            });
            toggle_rows[usize::from(!matches!(metric, Metric::HeartRate | Metric::Hrv | Metric::Spo2))].append(&toggle);
        }
        {
            let this = this.clone();
            calendar.connect_selected(move |day| this.select_day(day));
        }
        {
            let this = this.clone();
            calendar.connect_month_changed(move |_| this.load_marks());
        }
        for is_from in [true, false] {
            let this2 = this.clone();
            let row = if is_from { &this.inner.from_row } else { &this.inner.to_row };
            row.calendar.connect_day_selected(move |cal| {
                let Some(day) = calendar_date(cal) else { return };
                let inner = &this2.inner;
                if is_from {
                    inner.from.set(day);
                    inner.from_row.popover.popdown();
                    if inner.to.get() < day {
                        inner.to.set(day);
                    }
                } else {
                    inner.to.set(day);
                    inner.to_row.popover.popdown();
                    if inner.from.get() > day {
                        inner.from.set(day);
                    }
                }
                this2.load_charts();
            });
        }
        this.select_day(today);
        this
    }

    fn ring(&self) -> Option<(Rc<Store>, String)> {
        Some((self.inner.store.clone()?, self.inner.ring.borrow().clone()?))
    }

    pub fn refresh(&self) {
        self.load_marks();
        self.load_day(self.inner.calendar.selected());
        self.load_charts();
    }

    /// Opens on the newest day with data when today has none.
    pub fn open_latest(&self) {
        let Some((store, ring)) = self.ring() else { return };
        let today = Local::now().date_naive();
        let day = match store.history_span(&ring) {
            Ok(Some((_, newest))) if newest < local_midnight(today) => {
                Local.timestamp_opt(newest, 0).single().map(|t| t.date_naive()).unwrap_or(today)
            }
            _ => today,
        };
        self.inner.calendar.select(day);
        self.select_day(day);
    }

    /// Opens the full-day view of `day`.
    pub fn open_day(&self, day: NaiveDate) {
        self.inner.calendar.select(day);
        self.select_day(day);
        // Toggling reloads the day.
        self.inner.full_button.set_active(true);
    }

    /// Opens the Sleep view of the night that ended on `day`.
    pub fn open_night(&self, day: NaiveDate) {
        self.inner.calendar.select(day);
        self.select_day(day);
        if self.inner.sleep_button.is_active() {
            self.load_day(day);
        } else {
            // Toggling reloads the day.
            self.inner.sleep_button.set_active(true);
        }
    }

    fn select_day(&self, day: NaiveDate) {
        self.inner.from.set(day);
        self.inner.to.set(day);
        self.load_marks();
        self.load_day(day);
        self.load_charts();
    }

    fn load_marks(&self) {
        let Some((store, ring)) = self.ring() else { return };
        let month = self.inner.calendar.month();
        let next = first_of_month(month + Days::days(32));
        if let Ok(marks) = store.day_marks(&ring, local_midnight(month), local_midnight(next)) {
            self.inner.calendar.set_marks(marks);
        }
    }

    fn load_day(&self, day: NaiveDate) {
        let inner = &self.inner;
        for r in inner.day_rows.take() {
            inner.day_group.remove(&r);
        }
        inner.day_group.set_title(&day.format("%A, %-d %B").to_string());
        let Some((store, ring)) = self.ring() else { return };
        let (from, to) = (local_midnight(day), local_midnight(day + Days::days(1)));
        let mut rows = Vec::new();
        let part = inner.part.get();
        // Every sleep touching the day, and those that count toward it: a night belongs to the day
        // it ends on.
        let touching = store.sleep(&ring, from, to).unwrap_or_default();
        let own: Vec<(i64, i64)> = touching.iter().copied().filter(|(_, end)| *end > from && *end <= to).collect();
        let inside = |t: i64, spans: &[(i64, i64)]| spans.iter().any(|(a, b)| t >= *a && t < *b);
        let (start, end) = match part {
            DayPart::Asleep => (own.iter().map(|s| s.0).min().unwrap_or(from), to),
            _ => (from, to),
        };
        let series = |m| {
            let mut points = super::series(&store, &ring, m, start, end);
            match part {
                DayPart::Full => {}
                DayPart::Awake => points.retain(|p| !inside(p.0, &touching)),
                DayPart::Asleep => points.retain(|p| inside(p.0, &own)),
            }
            points
        };
        if part != DayPart::Awake && !own.is_empty() {
            let total: i64 = own.iter().map(|(a, b)| b - a).sum();
            let at = |u: i64| Local.timestamp_opt(u, 0).single().map(|t| t.format("%H:%M").to_string()).unwrap_or_default();
            let awake: i64 = own.iter().map(|(a, b)| store.awake(&ring, *a, *b).unwrap_or(0)).sum();
            rows.push(row("metric-sleep", "Sleep", &super::duration(total - awake), &super::sleep_subtitle(&own, awake, at)));
        }
        for metric in Metric::ALL {
            let points = series(metric);
            let Some(s) = Stats::of(points.iter().map(|p| p.1)) else { continue };
            let precision = metric.precision();
            let mut detail = format!("Low: {:.*} · High: {:.*}", precision, s.min, precision, s.max);
            if metric == Metric::HeartRate && part == DayPart::Full {
                if let Some(rest) = resting_heart_rate(&points) {
                    detail.push_str(&format!(" · Resting: {rest:.0}"));
                }
            }
            if part == DayPart::Asleep {
                detail.push_str(&sleep_detail(&store, &ring, metric, &points, start, s.mean));
            }
            rows.push(row(
                dots::metric(metric),
                metric.name(),
                &format!("{:.*} {}", precision, s.mean, super::metric_unit(metric)),
                &detail,
            ));
        }
        if part == DayPart::Full {
            if let Ok((steps, source)) = store.steps(&ring, from, to) {
                if steps > 0 {
                    let note = match source {
                        crate::store::StepSource::Ring => "",
                        crate::store::StepSource::Connected => "Counted while connected",
                    };
                    rows.push(row(dots::STEPS, "Steps", &steps.to_string(), note));
                }
            }
        }
        if let Some(suffix) = inner.day_group.header_suffix() {
            suffix.set_visible(!rows.is_empty());
        }
        if rows.is_empty() {
            let empty = if part == DayPart::Asleep { "No sleep estimated" } else { "No readings" };
            rows.push(adw::ActionRow::builder().title(empty).build());
        }
        for r in &rows {
            inner.day_group.add(r);
        }
        inner.day_rows.replace(rows);
    }

    fn load_charts(&self) {
        let inner = &self.inner;
        show_date(&inner.from_row, inner.from.get());
        show_date(&inner.to_row, inner.to.get());
        while let Some(child) = inner.charts.first_child() {
            inner.charts.remove(&child);
        }
        let Some((store, ring)) = self.ring() else { return };
        let mut to = local_midnight(inner.to.get() + Days::days(1));
        if inner.to.get() == Local::now().date_naive() {
            to = to.min(super::next_hour());
        }
        let from = local_midnight(inner.from.get());
        let sleep = store.sleep(&ring, from, to).unwrap_or_default();
        let streams = inner.streams.borrow();
        for metric in Metric::ALL.into_iter().filter(|m| streams.contains(m)) {
            let points = super::series(&store, &ring, metric, from, to);
            let p = metric.precision();
            let summary = Stats::of(points.iter().map(|p| p.1))
                .map(|s| format!("avg {:.*} · {:.*}–{:.*} {}", p, s.mean, p, s.min, p, s.max, super::metric_unit(metric)))
                .unwrap_or_default();
            let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            header.append(&dots::dot(dots::metric(metric)));
            header.append(&gtk::Label::builder().label(metric.name()).css_classes(["heading"]).hexpand(true).xalign(0.0).build());
            header.append(&gtk::Label::builder().label(summary).css_classes(["caption", "dim-label", "numeric"]).build());
            let chart = Chart::new(140);
            chart.set(metric, from, to, points, sleep.clone());
            let block = gtk::Box::new(gtk::Orientation::Vertical, 6);
            block.append(&header);
            block.append(&chart.area);
            inner.charts.append(&block);
        }
    }
}
