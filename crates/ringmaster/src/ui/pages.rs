//! The Sleep and Activity tabs: recent nights or days as bars against the goal, averages, and a
//! list that opens each one in History.

use super::bars::{Bar, Bars};
use super::dots;
use crate::store::{local_midnight, StepSource, Store};
use adw::prelude::*;
use chrono::{Duration as Days, Local, NaiveDate, TimeZone, Timelike};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

const SLEEP: (f64, f64, f64) = (0.369, 0.310, 0.839);
const STEPS: (f64, f64, f64) = (0.596, 0.416, 0.267);

/// Shared frame: a 2- or 4-week switch, the chart, averages and the list.
struct Frame {
    content: gtk::ScrolledWindow,
    days: Cell<i64>,
    bars: Bars,
    caption: gtk::Label,
    averages: adw::PreferencesGroup,
    list: adw::PreferencesGroup,
    rows: RefCell<Vec<adw::PreferencesRow>>,
}

impl Frame {
    fn new(list_title: &str, on_span: impl Fn() + Clone + 'static) -> Rc<Self> {
        let span_box = gtk::Box::builder().css_classes(["linked"]).halign(gtk::Align::Center).build();
        let two = gtk::ToggleButton::builder().label("2 weeks").active(true).build();
        let four = gtk::ToggleButton::builder().label("4 weeks").group(&two).build();
        span_box.append(&two);
        span_box.append(&four);
        let bars = Bars::new(160);
        let caption = gtk::Label::builder().css_classes(["caption", "dim-label"]).wrap(true).justify(gtk::Justification::Center).build();
        let card_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(12)
            .margin_bottom(10)
            .margin_start(6)
            .margin_end(6)
            .build();
        card_box.append(&span_box);
        card_box.append(&bars.area);
        card_box.append(&caption);
        let card = gtk::Box::builder().css_classes(["card"]).build();
        card_box.set_hexpand(true);
        card.append(&card_box);
        let averages = adw::PreferencesGroup::builder().title("Averages").build();
        let list = adw::PreferencesGroup::builder().title(list_title).build();
        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        column.append(&card);
        column.append(&averages);
        column.append(&list);
        let clamp = adw::Clamp::builder().maximum_size(600).child(&column).build();
        let content = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamp).vexpand(true).build();
        let frame = Rc::new(Self { content, days: Cell::new(14), bars, caption, averages, list, rows: Default::default() });
        for (button, days) in [(&two, 14), (&four, 28)] {
            let (frame, on_span) = (frame.clone(), on_span.clone());
            button.connect_toggled(move |b| {
                if b.is_active() {
                    frame.days.set(days);
                    on_span();
                }
            });
        }
        frame
    }

    fn clear(&self) {
        for r in self.rows.take() {
            if let Some(group) = r.ancestor(adw::PreferencesGroup::static_type()).and_downcast::<adw::PreferencesGroup>() {
                group.remove(&r);
            }
        }
    }

    fn add(&self, group: &adw::PreferencesGroup, row: impl IsA<adw::PreferencesRow> + IsA<gtk::Widget>) {
        group.add(&row);
        self.rows.borrow_mut().push(row.upcast());
    }

    /// The days shown, oldest first, ending today.
    fn range(&self) -> Vec<NaiveDate> {
        let today = Local::now().date_naive();
        (0..self.days.get()).rev().map(|i| today - Days::days(i)).collect()
    }
}

fn value_row(title: &str, value: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).subtitle(subtitle).build();
    row.add_suffix(&gtk::Label::builder().label(value).valign(gtk::Align::Center).css_classes(["numeric"]).build());
    row
}

fn day_row(dot: &str, day: NaiveDate, value: &str, subtitle: &str, open: &Rc<dyn Fn(NaiveDate)>) -> adw::ActionRow {
    let row = value_row(&day.format("%a %-d %b").to_string(), value, subtitle);
    row.add_prefix(&dots::dot(dot));
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    row.set_activatable(true);
    let open = open.clone();
    row.connect_activated(move |_| open(day));
    row
}

fn clock(unix: i64) -> String {
    Local.timestamp_opt(unix, 0).single().map(|t| t.format("%H:%M").to_string()).unwrap_or_default()
}

fn minutes_of_day(unix: i64) -> i64 {
    Local.timestamp_opt(unix, 0).single().map(|t| (t.hour() * 60 + t.minute()) as i64).unwrap_or(0)
}

/// Average time of day, counted from `pivot` minutes past midnight so times either side of
/// midnight average sensibly (noon for bedtimes, midnight for waking).
fn average_clock(minutes: &[i64], pivot: i64) -> Option<String> {
    if minutes.is_empty() {
        return None;
    }
    let shifted: i64 = minutes.iter().map(|m| (m - pivot).rem_euclid(1440)).sum::<i64>() / minutes.len() as i64;
    let m = (shifted + pivot).rem_euclid(1440);
    Some(format!("{:02}:{:02}", m / 60, m % 60))
}

/// Sleeps grouped by the local day they end on, the day a night belongs to.
fn by_end_day(spans: &[(i64, i64)], day_of: impl Fn(i64) -> NaiveDate) -> BTreeMap<NaiveDate, Vec<(i64, i64)>> {
    let mut nights: BTreeMap<NaiveDate, Vec<(i64, i64)>> = BTreeMap::new();
    for &(start, end) in spans {
        nights.entry(day_of(end)).or_default().push((start, end));
    }
    nights
}

fn local_day(unix: i64) -> NaiveDate {
    Local.timestamp_opt(unix, 0).single().map(|t| t.date_naive()).unwrap_or_default()
}

pub struct SleepPage {
    frame: Rc<Frame>,
    store: Option<Rc<Store>>,
    ring: Rc<RefCell<Option<String>>>,
    open: Rc<dyn Fn(NaiveDate)>,
}

impl SleepPage {
    pub fn new(store: Option<Rc<Store>>, ring: Rc<RefCell<Option<String>>>, open: impl Fn(NaiveDate) + 'static) -> Rc<Self> {
        let slot: Rc<RefCell<Option<std::rc::Weak<Self>>>> = Default::default();
        let s = slot.clone();
        let frame = Frame::new("Nights", move || {
            if let Some(page) = s.borrow().as_ref().and_then(|w| w.upgrade()) {
                page.refresh();
            }
        });
        let page = Rc::new(Self { frame, store, ring, open: Rc::new(open) });
        slot.replace(Some(Rc::downgrade(&page)));
        page.refresh();
        page
    }

    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.frame.content
    }

    pub fn refresh(&self) {
        let f = &self.frame;
        f.clear();
        let days = f.range();
        let (Some(store), Some(ring)) = (self.store.as_ref(), self.ring.borrow().clone()) else { return };
        let (from, to) = (local_midnight(days[0]), local_midnight(*days.last().unwrap() + Days::days(1)));
        let spans: Vec<(i64, i64)> = store.sleep(&ring, from, to).unwrap_or_default().into_iter().filter(|(_, end)| *end > from).collect();
        let nights = by_end_day(&spans, local_day);
        let goal_minutes = store.settings().unwrap_or_default().sleep_goal_minutes;
        let goal_h = goal_minutes as f64 / 60.0;
        // Hours asleep per day, with each night's window and minutes awake.
        let mut asleep: BTreeMap<NaiveDate, (f64, i64)> = BTreeMap::new();
        for (day, spans) in &nights {
            let awake: i64 = spans.iter().map(|(a, b)| store.awake(&ring, *a, *b).unwrap_or(0)).sum();
            let total: i64 = spans.iter().map(|(a, b)| b - a).sum();
            asleep.insert(*day, ((total - awake) as f64 / 3600.0, awake));
        }
        let bars = days.iter().map(|d| Bar { label: d.format("%-d").to_string(), value: asleep.get(d).map_or(0.0, |a| a.0) }).collect();
        f.bars.set(bars, Some(goal_h), SLEEP, |v| format!("{v:.0} h"));
        f.caption.set_text(&format!("Time asleep each night · dashed line: goal of {}", super::duration(goal_minutes as i64 * 60)));

        let hours: Vec<f64> = asleep.values().map(|a| a.0).collect();
        if hours.is_empty() {
            f.add(&f.averages, value_row("No nights estimated in this period", "", ""));
        } else {
            let mean = hours.iter().sum::<f64>() / hours.len() as f64;
            let at_goal = hours.iter().filter(|h| **h >= goal_h).count();
            f.add(&f.averages, value_row("Time asleep", &format!("{mean:.1} h"), &format!("Goal {goal_h:.1} h")));
            // A night's start is its first sleep, its end the last.
            let starts: Vec<i64> = nights.values().filter_map(|s| s.iter().map(|x| x.0).min()).map(minutes_of_day).collect();
            let ends: Vec<i64> = nights.values().filter_map(|s| s.iter().map(|x| x.1).max()).map(minutes_of_day).collect();
            if let Some(t) = average_clock(&starts, 12 * 60) {
                f.add(&f.averages, value_row("Fell asleep", &t, ""));
            }
            if let Some(t) = average_clock(&ends, 0) {
                f.add(&f.averages, value_row("Woke up", &t, ""));
            }
            f.add(&f.averages, value_row("Nights at goal", &format!("{at_goal} of {}", hours.len()), ""));
        }

        for (day, spans) in nights.iter().rev() {
            let (hours, awake) = asleep[day];
            let subtitle = super::sleep_subtitle(spans, awake, clock);
            f.add(&f.list, day_row("metric-sleep", *day, &format!("{hours:.1} h"), &subtitle, &self.open));
        }
    }
}

pub struct ActivityPage {
    frame: Rc<Frame>,
    store: Option<Rc<Store>>,
    ring: Rc<RefCell<Option<String>>>,
    open: Rc<dyn Fn(NaiveDate)>,
}

impl ActivityPage {
    pub fn new(store: Option<Rc<Store>>, ring: Rc<RefCell<Option<String>>>, open: impl Fn(NaiveDate) + 'static) -> Rc<Self> {
        let slot: Rc<RefCell<Option<std::rc::Weak<Self>>>> = Default::default();
        let s = slot.clone();
        let frame = Frame::new("Days", move || {
            if let Some(page) = s.borrow().as_ref().and_then(|w| w.upgrade()) {
                page.refresh();
            }
        });
        let page = Rc::new(Self { frame, store, ring, open: Rc::new(open) });
        slot.replace(Some(Rc::downgrade(&page)));
        page.refresh();
        page
    }

    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.frame.content
    }

    pub fn refresh(&self) {
        let f = &self.frame;
        f.clear();
        let days = f.range();
        let (Some(store), Some(ring)) = (self.store.as_ref(), self.ring.borrow().clone()) else { return };
        let steps: Vec<(NaiveDate, i64, StepSource)> = days
            .iter()
            .map(|d| {
                let (n, source) =
                    store.steps(&ring, local_midnight(*d), local_midnight(*d + Days::days(1))).unwrap_or((0, StepSource::Ring));
                (*d, n, source)
            })
            .collect();
        let goal_steps = store.settings().unwrap_or_default().step_goal;
        let goal = goal_steps as f64;
        let bars = steps.iter().map(|(d, n, _)| Bar { label: d.format("%-d").to_string(), value: *n as f64 }).collect();
        f.bars.set(bars, Some(goal), STEPS, |v| if v >= 1000.0 { format!("{:.0}k", v / 1000.0) } else { format!("{v:.0}") });
        f.caption.set_text(&format!("Steps each day · dashed line: goal of {goal_steps}"));

        let counted: Vec<i64> = steps.iter().filter(|s| s.1 > 0).map(|s| s.1).collect();
        if counted.is_empty() {
            f.add(&f.averages, value_row("No steps recorded in this period", "", ""));
        } else {
            let mean = counted.iter().sum::<i64>() / counted.len() as i64;
            let at_goal = counted.iter().filter(|n| **n as f64 >= goal).count();
            f.add(&f.averages, value_row("Steps a day", &mean.to_string(), &format!("Goal {goal_steps}")));
            f.add(&f.averages, value_row("Days at goal", &format!("{at_goal} of {}", counted.len()), ""));
            if let Some((day, best, _)) = steps.iter().max_by_key(|s| s.1) {
                f.add(&f.averages, value_row("Best day", &best.to_string(), &day.format("%A %-d %B").to_string()));
            }
        }
        for (day, n, source) in steps.iter().rev().filter(|s| s.1 > 0) {
            let note = match source {
                StepSource::Ring => "",
                StepSource::Connected => "Counted while connected",
            };
            f.add(&f.list, day_row(dots::STEPS, *day, &n.to_string(), note, &self.open));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_average_across_midnight() {
        assert_eq!(average_clock(&[23 * 60, 60], 12 * 60).as_deref(), Some("00:00"));
        assert_eq!(average_clock(&[6 * 60, 7 * 60], 0).as_deref(), Some("06:30"));
        assert_eq!(average_clock(&[], 0), None);
    }

    #[test]
    fn nights_belong_to_the_day_they_end() {
        let day = |u: i64| NaiveDate::from_ymd_opt(2026, 11, 1).unwrap() + Days::days(u / 100);
        let n = by_end_day(&[(50, 120), (130, 150), (180, 250)], day);
        assert_eq!(n.len(), 2);
        assert_eq!(n[&day(150)], vec![(50, 120), (130, 150)]);
    }
}
