//! Month calendar with a coloured dot per kind of data recorded on each day.

use crate::store::{DayMarks, Metric};
use chrono::{Datelike, Duration as Days, Local, NaiveDate};
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

type Callback = RefCell<Option<Box<dyn Fn(NaiveDate)>>>;

struct State {
    month: Cell<NaiveDate>,
    selected: Cell<NaiveDate>,
    marks: RefCell<HashMap<String, DayMarks>>,
    on_select: Callback,
    on_month: Callback,
    title: gtk::Label,
    grid: gtk::Grid,
}

#[derive(Clone)]
pub struct MonthCalendar {
    pub widget: gtk::Box,
    state: Rc<State>,
}

pub fn first_of_month(day: NaiveDate) -> NaiveDate {
    day.with_day(1).unwrap()
}

fn next_month(first: NaiveDate) -> NaiveDate {
    first_of_month(first + Days::days(32))
}

fn previous_month(first: NaiveDate) -> NaiveDate {
    first_of_month(first - Days::days(1))
}

/// Dot classes in display order.
pub fn mark_classes(marks: &DayMarks) -> Vec<&'static str> {
    let mut classes: Vec<&'static str> = Metric::ALL.into_iter().filter(|m| marks.metrics.contains(m)).map(|m| m.style().0).collect();
    if marks.workout {
        classes.push("metric-workout");
    }
    classes
}

impl MonthCalendar {
    pub fn new() -> Self {
        let today = Local::now().date_naive();
        let title = gtk::Label::builder().hexpand(true).css_classes(["heading"]).build();
        let prev = gtk::Button::builder().icon_name("go-previous-symbolic").tooltip_text("Previous month").css_classes(["flat"]).build();
        let next = gtk::Button::builder().icon_name("go-next-symbolic").tooltip_text("Next month").css_classes(["flat"]).build();
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.append(&prev);
        header.append(&title);
        header.append(&next);
        let grid = gtk::Grid::builder().column_homogeneous(true).row_spacing(2).column_spacing(2).build();
        let widget = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).css_classes(["card", "calendar"]).build();
        widget.append(&header);
        widget.append(&grid);

        let state = Rc::new(State {
            month: Cell::new(first_of_month(today)),
            selected: Cell::new(today),
            marks: Default::default(),
            on_select: Default::default(),
            on_month: Default::default(),
            title,
            grid,
        });
        let cal = Self { widget, state };
        for (button, forward) in [(prev, false), (next, true)] {
            let cal = cal.clone();
            button.connect_clicked(move |_| {
                let m = cal.state.month.get();
                cal.state.month.set(if forward { next_month(m) } else { previous_month(m) });
                cal.rebuild();
                if let Some(f) = cal.state.on_month.borrow().as_ref() {
                    f(cal.state.month.get());
                }
            });
        }
        cal.rebuild();
        cal
    }

    pub fn month(&self) -> NaiveDate {
        self.state.month.get()
    }

    pub fn selected(&self) -> NaiveDate {
        self.state.selected.get()
    }

    pub fn connect_selected(&self, f: impl Fn(NaiveDate) + 'static) {
        self.state.on_select.replace(Some(Box::new(f)));
    }

    pub fn connect_month_changed(&self, f: impl Fn(NaiveDate) + 'static) {
        self.state.on_month.replace(Some(Box::new(f)));
    }

    pub fn set_marks(&self, marks: HashMap<String, DayMarks>) {
        self.state.marks.replace(marks);
        self.rebuild();
    }

    pub fn select(&self, day: NaiveDate) {
        self.state.selected.set(day);
        self.state.month.set(first_of_month(day));
        self.rebuild();
    }

    fn rebuild(&self) {
        let s = &self.state;
        while let Some(child) = s.grid.first_child() {
            s.grid.remove(&child);
        }
        let month = s.month.get();
        s.title.set_text(&month.format("%B %Y").to_string());
        for (col, name) in ["S", "M", "T", "W", "T", "F", "S"].iter().enumerate() {
            s.grid.attach(&gtk::Label::builder().label(*name).css_classes(["caption", "dim-label"]).build(), col as i32, 0, 1, 1);
        }
        let today = Local::now().date_naive();
        let offset = month.weekday().num_days_from_sunday() as i32;
        let marks = s.marks.borrow();
        let mut day = month;
        while day.month() == month.month() {
            let cell = offset + day.day0() as i32;
            let number = gtk::Label::builder().label(day.day().to_string()).css_classes(["day-number"]).build();
            let dots = gtk::Box::builder().halign(gtk::Align::Center).spacing(1).height_request(8).build();
            if let Some(m) = marks.get(&day.format("%Y-%m-%d").to_string()) {
                for class in mark_classes(m) {
                    dots.append(&gtk::Label::builder().label("●").css_classes(["calendar-dot", class]).build());
                }
            }
            let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
            content.append(&number);
            content.append(&dots);
            let button = gtk::Button::builder().child(&content).css_classes(["flat", "calendar-day"]).sensitive(day <= today).build();
            if day == s.selected.get() {
                button.add_css_class("selected-day");
            }
            if day == today {
                button.add_css_class("today");
            }
            let cal = self.clone();
            let clicked = day;
            button.connect_clicked(move |_| {
                cal.state.selected.set(clicked);
                cal.rebuild();
                if let Some(f) = cal.state.on_select.borrow().as_ref() {
                    f(clicked);
                }
            });
            s.grid.attach(&button, cell % 7, 1 + cell / 7, 1, 1);
            day += Days::days(1);
        }
    }
}
