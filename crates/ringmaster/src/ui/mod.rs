mod bars;
mod calendar;
mod chart;
mod dots;
mod history;
mod pages;
mod preferences;
mod pull_refresh;
mod rings;

use crate::backend::{Command, Event, LinkState, MeasureKind, RingChoice};
use crate::settings::{to_fahrenheit, Settings};
use crate::store::{Metric, Reading, Spot, Stats, StepSource, Store, Summary};
use adw::prelude::*;
use chart::{local_midnight, Chart};
use chrono::{DateTime, Duration as Days, Local, TimeZone, Timelike};
use history::HistoryPage;
use ringconn::records::{Descriptor, RingState};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

struct Measure {
    heart_rate: gtk::ToggleButton,
    spo2: gtk::ToggleButton,
    value: gtk::Label,
    status: gtk::Label,
    progress: gtk::ProgressBar,
    button: gtk::Button,
    active: Cell<Option<MeasureKind>>,
    /// When the first reading arrived; until then the bar bounces.
    locked: Rc<Cell<Option<std::time::Instant>>>,
    animation: RefCell<Option<glib::SourceId>>,
    /// Value and status text per kind for as long as the sheet stays open.
    session: RefCell<[Option<(String, String)>; 2]>,
    open: Cell<bool>,
}

/// Time covered by the main page's trend chart.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Span {
    Hour,
    Day,
    Week,
}

struct Week {
    chart: Chart,
    metric: Cell<Metric>,
    span: Cell<Span>,
    summary: gtk::Label,
}

struct Widgets {
    toasts: adw::ToastOverlay,
    ring_row: adw::ActionRow,
    /// Shows the ring row opens the ring picker; only with more than one ring.
    ring_switch: gtk::Image,
    connect: gtk::Button,
    battery_box: gtk::Box,
    quick_row: adw::ActionRow,
    battery_icon: gtk::Image,
    battery: gtk::Label,
    sync_row: adw::ActionRow,
    sync: gtk::Button,
    progress: gtk::ProgressBar,
    pull: Rc<pull_refresh::PullRefresh>,
    heart_rate: (adw::ActionRow, gtk::Label),
    hrv: (adw::ActionRow, gtk::Label),
    spo2: (adw::ActionRow, gtk::Label),
    resp: (adw::ActionRow, gtk::Label),
    temperature: (adw::ActionRow, gtk::Label),
    steps: (adw::ActionRow, gtk::Label),
    sleep: (adw::ActionRow, gtk::Label),
    measure: Measure,
    week: Week,
    history: HistoryPage,
    sleep_page: Rc<pages::SleepPage>,
    activity_page: Rc<pages::ActivityPage>,
    stack: adw::ViewStack,
    store: Option<Rc<Store>>,
    ring: Rc<RefCell<Option<String>>>,
    connected: Cell<bool>,
    progress_total: Cell<u32>,
    sync_pulse: RefCell<Option<glib::SourceId>>,
    ring_name: RefCell<Option<String>>,
    app: adw::Application,
    settings: RefCell<Settings>,
    case_row: adw::ActionRow,
    case_icon: gtk::Image,
    case_value: gtk::Label,
    /// The latest status frame and when it arrived.
    last_telemetry: RefCell<Option<(i64, Descriptor)>>,
    /// Newest skin temperature from history, as (unix, degC).
    history_temperature: std::cell::Cell<Option<(i64, f64)>>,
    /// Last stored case reading, as (unix, percent, plugged in).
    stored_case: std::cell::Cell<Option<(i64, u8, bool)>>,
    /// The night shown in the Sleep row.
    last_night: Cell<Option<(i64, i64)>>,
}

fn value_row(title: &str, dot: Option<&str>) -> (adw::ActionRow, gtk::Label) {
    let row = adw::ActionRow::builder().title(title).build();
    if let Some(class) = dot {
        // As wide as an icon, so titles line up with the Quick reading row.
        let dot = dots::dot(class);
        dot.set_width_request(16);
        row.add_prefix(&dot);
    }
    let label = gtk::Label::builder().label("–").valign(gtk::Align::Center).css_classes(["numeric", "title-4"]).build();
    row.add_suffix(&label);
    (row, label)
}

/// Presents `dialog` as a sheet sliding up from the bottom on narrow windows.
pub(super) fn present_sheet(dialog: &adw::Dialog, from: &impl IsA<gtk::Widget>) {
    // Automatic placement only slides up below 450 px, and phones such as the FLX1s are wider.
    // The mode exists from libadwaita 1.6 and the build targets 1.5, so it is set by name.
    if let Some(spec) = dialog.find_property("presentation-mode") {
        let narrow = from.root().is_some_and(|r| r.width() < 720);
        let mode = glib::EnumClass::with_type(spec.value_type())
            .and_then(|class| class.to_value_by_nick(if narrow { "bottom-sheet" } else { "auto" }));
        if let Some(mode) = mode {
            dialog.set_property_from_value("presentation-mode", &mode);
        }
    }
    dialog.present(Some(from));
}

fn when(unix: i64) -> String {
    let Some(t): Option<DateTime<Local>> = Local.timestamp_opt(unix, 0).single() else { return String::new() };
    let today = Local::now().date_naive();
    match (today - t.date_naive()).num_days() {
        0 => t.format("Today %H:%M").to_string(),
        1 => t.format("Yesterday %H:%M").to_string(),
        _ => t.format("%-d %b %H:%M").to_string(),
    }
}

/// "14:32".
fn clock(unix: i64) -> String {
    Local.timestamp_opt(unix, 0).single().map(|t| t.format("%H:%M").to_string()).unwrap_or_default()
}

fn unit(kind: MeasureKind) -> &'static str {
    match kind {
        MeasureKind::HeartRate => "bpm",
        MeasureKind::Spo2 => "%",
    }
}

thread_local! {
    static FAHRENHEIT: Cell<bool> = const { Cell::new(false) };
}

/// A stored series in display units.
pub(super) fn series(store: &Store, ring: &str, metric: Metric, from: i64, to: i64) -> Vec<(i64, f64)> {
    let mut points = store.series(ring, metric, from, to).unwrap_or_default();
    if metric == Metric::SkinTemp && FAHRENHEIT.get() {
        points.iter_mut().for_each(|p| p.1 = to_fahrenheit(p.1));
    }
    points
}

pub(super) fn metric_unit(metric: Metric) -> &'static str {
    if metric == Metric::SkinTemp && FAHRENHEIT.get() {
        "°F"
    } else {
        metric.unit()
    }
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        add_icons(&display);
    }
}

const ICONS: [(&str, &str); 3] = [
    ("ringmaster-stopwatch-symbolic.svg", include_str!("../../../../data/icons/ringmaster-stopwatch-symbolic.svg")),
    ("ringmaster-ring-symbolic.svg", include_str!("../../../../data/net.dunevent.Ringmaster-symbolic.svg")),
    ("ringmaster-steps-symbolic.svg", include_str!("../../../../data/icons/ringmaster-steps-symbolic.svg")),
];

/// Writes the app's own icons to the cache and adds them to the icon theme, which works the same
/// for installed, Flatpak and `cargo run` builds without compiling a resource bundle.
fn add_icons(display: &gtk::gdk::Display) {
    // Inside the hicolor layout, not directly in the search path: GTK 4.14 recolours a symbolic
    // icon for dark mode only when it comes from a theme directory. The system's hicolor index
    // already lists `scalable/actions`.
    let root = glib::user_cache_dir().join("ringmaster").join("icons");
    let dir = root.join("hicolor").join("scalable").join("actions");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    // Earlier versions wrote the icons directly in `root`; those copies are removed.
    for (name, _) in ICONS {
        let _ = std::fs::remove_file(root.join(name));
    }
    for (name, svg) in ICONS {
        let path = dir.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(svg) {
            let _ = std::fs::write(&path, svg);
        }
    }
    gtk::IconTheme::for_display(display).add_search_path(&root);
}

fn linked_toggles(items: &[(&str, &str)]) -> (gtk::Box, Vec<gtk::ToggleButton>) {
    let container = gtk::Box::builder().css_classes(["linked"]).halign(gtk::Align::Center).build();
    let mut buttons: Vec<gtk::ToggleButton> = Vec::new();
    for (class, label) in items {
        let b = gtk::ToggleButton::builder().child(&dots::labeled(class, label)).build();
        if let Some(first) = buttons.first() {
            b.set_group(Some(first));
        }
        container.append(&b);
        buttons.push(b);
    }
    buttons[0].set_active(true);
    (container, buttons)
}

/// First widget below `root`, depth first, with the CSS class `class`.
fn descendant_with_class(root: &gtk::Widget, class: &str) -> Option<gtk::Widget> {
    let mut child = root.first_child();
    while let Some(c) = child {
        if c.has_css_class(class) {
            return Some(c);
        }
        if let Some(found) = descendant_with_class(&c, class) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}

/// Linked toggles in two groups, before and after `split`, drawn as one group; `stack_to_fit`
/// stacks them when one row does not fit.
fn split_toggles(items: &[(&str, &str)], split: usize) -> (gtk::Box, Vec<gtk::ToggleButton>) {
    let container = gtk::Box::builder().halign(gtk::Align::Center).css_classes(["joined"]).build();
    let (first, mut buttons) = linked_toggles(&items[..split]);
    let (second, rest) = linked_toggles(&items[split..]);
    rest[0].set_active(false);
    for b in &rest {
        b.set_group(buttons.first());
    }
    buttons[0].set_active(true);
    buttons.extend(rest);
    container.append(&first);
    container.append(&second);
    (container, buttons)
}

/// Stacks the groups of `row` when the width given to `beside`, a widget in the same column,
/// is less than the row needs. The row's own measurements decide, so text and display scaling
/// are both accounted for.
fn stack_to_fit(row: &gtk::Box, beside: &gtk::DrawingArea) {
    let row = row.clone();
    beside.connect_resize(move |_, width, _| {
        let mut need = 0;
        let mut child = row.first_child();
        while let Some(c) = child {
            need += c.measure(gtk::Orientation::Horizontal, -1).1;
            child = c.next_sibling();
        }
        let stacked = width < need;
        if stacked == (row.orientation() == gtk::Orientation::Vertical) {
            return;
        }
        // Changing the layout during allocation is deferred to the next main loop pass.
        let row = row.clone();
        glib::idle_add_local_once(move || {
            row.set_orientation(if stacked { gtk::Orientation::Vertical } else { gtk::Orientation::Horizontal });
            row.set_css_classes(&[if stacked { "stacked" } else { "joined" }]);
        });
    });
}

pub fn build_window(
    app: &adw::Application,
    commands: async_channel::Sender<Command>,
    events: async_channel::Receiver<Event>,
    store: Option<Rc<Store>>,
) -> adw::ApplicationWindow {
    load_css();
    let send = {
        let commands = commands.clone();
        move |c: Command| {
            let _ = commands.try_send(c);
        }
    };

    let connect = gtk::Button::builder().label("Connect").valign(gtk::Align::Center).css_classes(["suggested-action"]).build();
    let (battery_box, battery_icon, battery) = battery_indicator();
    battery_box.set_margin_end(6);
    battery_box.set_visible(false);
    let ring_row = adw::ActionRow::builder().title("No ring yet").subtitle("Not connected").build();
    let ring_switch = gtk::Image::builder().icon_name("pan-down-symbolic").visible(false).build();
    ring_row.add_suffix(&ring_switch);
    ring_row.add_suffix(&battery_box);
    ring_row.add_suffix(&connect);
    let sync = gtk::Button::builder().label("Sync").valign(gtk::Align::Center).sensitive(false).build();
    let sync_row = adw::ActionRow::builder().title("Last sync").subtitle("Never").build();
    // Suffixes do not stretch, so the bar needs a width of its own; `hexpand` lets it take the
    // spare width while it shows.
    let progress = gtk::ProgressBar::builder()
        .visible(false)
        .width_request(100)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .margin_end(6)
        .css_classes(["sync-bar"])
        .build();
    sync_row.add_suffix(&progress);
    // The row's title box normally takes all spare width; while the bar shows, the bar takes it.
    if let Some(title) = descendant_with_class(sync_row.upcast_ref(), "title") {
        progress.connect_visible_notify(move |bar| title.set_hexpand(!bar.is_visible()));
    }
    sync_row.add_suffix(&sync);
    let ring_group = adw::PreferencesGroup::new();
    ring_group.add(&ring_row);
    ring_group.add(&sync_row);
    let (case_box, case_icon, case_value) = battery_indicator();
    let case_row = adw::ActionRow::builder().title("Charging case").visible(false).build();
    case_row.add_suffix(&case_box);
    ring_group.add(&case_row);

    let heart_rate = value_row("Heart rate", Some(dots::metric(Metric::HeartRate)));
    let hrv = value_row("Heart rate variability", Some(dots::metric(Metric::Hrv)));
    let spo2 = value_row("Blood oxygen", Some(dots::metric(Metric::Spo2)));
    let resp = value_row("Respiratory rate", Some(dots::metric(Metric::RespRate)));
    let temperature = value_row("Skin temperature", Some(dots::TEMPERATURE));
    let steps = value_row("Steps", Some(dots::STEPS));
    steps.1.set_css_classes(&["numeric", "heading"]);
    let sleep = value_row("Sleep", Some("metric-sleep"));
    let readings_group = adw::PreferencesGroup::builder().title("Latest readings").build();
    for (row, _) in [&heart_rate, &hrv, &spo2, &resp, &temperature, &steps, &sleep] {
        readings_group.add(row);
    }

    let (kind_box, kinds) =
        linked_toggles(&[(dots::measure(MeasureKind::HeartRate), "Heart rate"), (dots::measure(MeasureKind::Spo2), "Blood oxygen")]);
    let measure = Measure {
        heart_rate: kinds[0].clone(),
        spo2: kinds[1].clone(),
        value: gtk::Label::builder().label("–").css_classes(["title-1", "numeric"]).margin_top(6).build(),
        status: gtk::Label::builder()
            .visible(false)
            .css_classes(["caption", "dim-label"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build(),
        progress: gtk::ProgressBar::builder().visible(false).margin_start(24).margin_end(24).build(),
        button: gtk::Button::builder().halign(gtk::Align::Center).css_classes(["pill", "suggested-action"]).sensitive(false).build(),
        active: Cell::new(None),
        locked: Default::default(),
        animation: Default::default(),
        session: Default::default(),
        open: Cell::new(false),
    };
    set_measuring(&measure, None);
    let measure_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_top(12).margin_bottom(12).build();
    for widget in [
        kind_box.upcast_ref::<gtk::Widget>(),
        measure.value.upcast_ref(),
        measure.status.upcast_ref(),
        measure.progress.upcast_ref(),
        measure.button.upcast_ref(),
    ] {
        measure_box.append(widget);
    }
    measure_box.set_margin_start(12);
    measure_box.set_margin_end(12);
    measure_box.set_margin_bottom(24);
    let measure_view = adw::ToolbarView::new();
    measure_view.add_top_bar(&adw::HeaderBar::new());
    measure_view.set_content(Some(&measure_box));
    // Kept and re-presented, so a reading carries on while the dialog is closed.
    let measure_dialog = adw::Dialog::builder().title("Quick reading").content_width(360).child(&measure_view).build();
    let quick_row =
        adw::ActionRow::builder().title("Quick reading").subtitle("Heart rate or blood oxygen, about 30 seconds").activatable(true).build();
    quick_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    quick_row.add_prefix(&gtk::Image::from_icon_name("ringmaster-stopwatch-symbolic"));
    quick_row.set_sensitive(false);
    readings_group.add(&quick_row);
    {
        let dialog = measure_dialog.clone();
        quick_row.connect_activated(move |row| present_sheet(&dialog, row));
    }

    // Splits after SpO2, so a narrow window shows HR, HRV and SpO2 above Resp and Temp.
    let (stream_box, streams) = split_toggles(&Metric::ALL.map(|m| (dots::metric(m), m.short_name())), 3);
    let week = Week {
        chart: Chart::new(160),
        metric: Cell::new(Metric::HeartRate),
        span: Cell::new(Span::Day),
        summary: gtk::Label::builder()
            .css_classes(["caption", "dim-label", "numeric"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build(),
    };
    let week_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    let span_box = gtk::Box::builder().css_classes(["linked"]).halign(gtk::Align::Center).build();
    let past_hour = gtk::ToggleButton::builder().label("Past hour").build();
    let past_day = gtk::ToggleButton::builder().label("Past day").active(true).group(&past_hour).build();
    let past_week = gtk::ToggleButton::builder().label("Past week").group(&past_hour).build();
    span_box.append(&past_hour);
    span_box.append(&past_day);
    span_box.append(&past_week);
    week_box.append(&span_box);
    // Without this the row's one-line width would hold the window open, and it would never stack.
    let stream_clip = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::External)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_width(true)
        .child(&stream_box)
        .build();
    week_box.append(&stream_clip);
    week_box.append(&week.chart.area);
    week_box.append(&week.summary);
    let week_frame = gtk::Box::builder().css_classes(["card"]).build();
    week_frame.append(&week_box);
    week_box.set_hexpand(true);
    let week_group = adw::PreferencesGroup::builder().title("Trends").build();
    week_group.add(&week_frame);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(24)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    for group in [&ring_group, &readings_group, &week_group] {
        content.append(group);
    }

    let clamp = adw::Clamp::builder().maximum_size(600).child(&content).build();
    let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamp).vexpand(true).build();
    // A pull syncs whenever the Sync button could: a ring to connect to and no sync running. The
    // backend connects first when needed.
    let pull = {
        let (progress, store, send, sync) = (progress.clone(), store.clone(), send.clone(), sync.clone());
        pull_refresh::PullRefresh::new(
            &scroller,
            move || !progress.is_visible() && store.as_ref().is_some_and(|s| s.ring_to_connect().ok().flatten().is_some()),
            move || {
                sync.set_sensitive(false);
                send(Command::Sync);
            },
        )
    };
    let page_column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    page_column.append(&pull.indicator);
    page_column.append(&scroller);
    let initial_settings = store.as_ref().and_then(|s| s.settings().ok()).unwrap_or_default();
    FAHRENHEIT.set(initial_settings.fahrenheit);
    let ring: Rc<RefCell<Option<String>>> = Default::default();
    let history = HistoryPage::new(store.clone(), ring.clone());
    let stack = adw::ViewStack::new();
    // Sleep and Activity open a night or a day in History.
    let open_in_history = |sleep: bool| {
        let (history, stack) = (history.clone(), stack.clone());
        move |day: chrono::NaiveDate| {
            if sleep {
                history.open_night(day);
            } else {
                history.open_day(day);
            }
            stack.set_visible_child_name("history");
        }
    };
    let sleep_page = pages::SleepPage::new(store.clone(), ring.clone(), open_in_history(true));
    let activity_page = pages::ActivityPage::new(store.clone(), ring.clone(), open_in_history(false));
    stack.add_titled_with_icon(&page_column, Some("latest"), "Latest", "ringmaster-ring-symbolic");
    stack.add_titled_with_icon(&history.content, Some("history"), "History", "x-office-calendar-symbolic");
    stack.add_titled_with_icon(sleep_page.widget(), Some("sleep"), "Sleep", "weather-clear-night-symbolic");
    stack.add_titled_with_icon(activity_page.widget(), Some("activity"), "Activity", "ringmaster-steps-symbolic");
    {
        let (sleep_page, activity_page) = (sleep_page.clone(), activity_page.clone());
        stack.connect_visible_child_name_notify(move |stack| match stack.visible_child_name().as_deref() {
            Some("sleep") => sleep_page.refresh(),
            Some("activity") => activity_page.refresh(),
            _ => {}
        });
    }

    let toolbar = adw::ToolbarView::new();
    let menu = gio::Menu::new();
    menu.append(Some("Preferences"), Some("app.preferences"));
    menu.append(Some("About Ringmaster"), Some("app.about"));
    menu.append(Some("Quit"), Some("app.quit"));
    let switcher = adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build();
    let header = adw::HeaderBar::builder().title_widget(&switcher).build();
    header.pack_end(&gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).tooltip_text("Main menu").build());
    let switcher_bar = adw::ViewSwitcherBar::builder().stack(&stack).build();
    toolbar.add_top_bar(&header);
    toolbar.add_bottom_bar(&switcher_bar);
    toolbar.set_content(Some(&stack));
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&toolbar));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Ringmaster")
        .default_width(360)
        .default_height(720)
        .content(&toasts)
        .build();
    stack_to_fit(&stream_box, &week.chart.area);
    // Narrow windows get the tabs in a bar at the bottom and the app's name in the header.
    let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        550.0,
        adw::LengthUnit::Sp,
    ));
    let title = adw::WindowTitle::new("Ringmaster", "");
    narrow.add_setter(&header, "title-widget", Some(&title.to_value()));
    narrow.add_setter(&switcher_bar, "reveal", Some(&true.to_value()));
    window.add_breakpoint(narrow);
    let w = Rc::new(Widgets {
        toasts,
        ring_row,
        ring_switch,
        connect: connect.clone(),
        battery_box,
        quick_row: quick_row.clone(),
        battery_icon,
        battery,
        sync_row,
        sync: sync.clone(),
        progress,
        pull,
        heart_rate,
        hrv,
        spo2,
        resp,
        temperature,
        steps,
        sleep,
        measure,
        week,
        history,
        sleep_page,
        activity_page,
        stack: stack.clone(),
        store,
        ring,
        connected: Cell::new(false),
        progress_total: Cell::new(0),
        sync_pulse: Default::default(),
        ring_name: RefCell::new(None),
        app: app.clone(),
        settings: RefCell::new(initial_settings),
        case_row,
        case_icon,
        case_value,
        last_telemetry: Default::default(),
        history_temperature: Default::default(),
        stored_case: Default::default(),
        last_night: Default::default(),
    });
    w.sleep.0.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    {
        let w2 = w.clone();
        w.sleep.0.connect_activated(move |_| {
            let Some((_, end)) = w2.last_night.get() else { return };
            let Some(day) = Local.timestamp_opt(end, 0).single().map(|t| t.date_naive()) else { return };
            w2.history.open_night(day);
            w2.stack.set_visible_child_name("history");
        });
    }

    {
        let w2 = w.clone();
        quick_row.connect_activated(move |_| w2.measure.open.set(true));
    }
    {
        let (w2, send) = (w.clone(), send.clone());
        w.ring_row.connect_activated(move |_| pick_ring(&w2, send.clone()));
    }
    {
        let w2 = w.clone();
        measure_dialog.connect_closed(move |_| {
            w2.measure.open.set(false);
            w2.measure.session.replace(Default::default());
            show_session(&w2.measure);
        });
    }
    {
        let (w, send) = (w.clone(), send.clone());
        connect.connect_clicked(move |b| {
            b.set_sensitive(false);
            if !w.connected.get() {
                w.ring_row.set_subtitle("Connecting…");
            }
            send(if w.connected.get() { Command::Disconnect } else { Command::Connect });
        });
    }
    {
        let send = send.clone();
        sync.connect_clicked(move |b| {
            b.set_sensitive(false);
            send(Command::Sync);
        });
    }
    {
        let (w2, send) = (w.clone(), send.clone());
        w.measure.button.connect_clicked(move |_| {
            if w2.measure.active.get().is_some() {
                send(Command::StopMeasure);
            } else {
                send(Command::Measure(selected_kind(&w2.measure)));
            }
        });
    }
    // The group deactivates the old button before activating the new one, so only the newly
    // active button sees the final selection.
    for button in [&w.measure.heart_rate, &w.measure.spo2] {
        let w2 = w.clone();
        button.connect_toggled(move |b| {
            if b.is_active() {
                show_session(&w2.measure);
            }
        });
    }
    // The group deactivates the old button before activating the new one, so only the newly
    // active button reloads.
    for (button, span) in [(&past_hour, Span::Hour), (&past_day, Span::Day), (&past_week, Span::Week)] {
        let w2 = w.clone();
        button.connect_toggled(move |b| {
            if b.is_active() {
                w2.week.span.set(span);
                load_week(&w2);
            }
        });
    }
    for (button, metric) in streams.iter().zip(Metric::ALL) {
        let w2 = w.clone();
        button.connect_toggled(move |b| {
            if b.is_active() {
                w2.week.metric.set(metric);
                load_week(&w2);
            }
        });
    }

    let w_events = w.clone();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(event) = events.recv().await {
            apply(&w_events, event, &send);
        }
    });

    {
        let (w2, commands) = (w.clone(), commands.clone());
        window.connect_close_request(move |window| {
            if w2.settings.borrow().run_in_background {
                window.set_visible(false);
                let _ = commands.try_send(Command::WindowVisible(false));
                return glib::Propagation::Stop;
            }
            let _ = commands.try_send(Command::Shutdown);
            glib::Propagation::Proceed
        });
    }
    {
        let commands = commands.clone();
        window.connect_visible_notify(move |window| {
            if window.is_visible() {
                let _ = commands.try_send(Command::WindowVisible(true));
            }
        });
    }
    add_actions(app, &window, &w, commands);
    window
}

fn add_actions(app: &adw::Application, window: &adw::ApplicationWindow, w: &Rc<Widgets>, commands: async_channel::Sender<Command>) {
    let preferences = gio::SimpleAction::new("preferences", None);
    {
        let (window, w, commands) = (window.clone(), w.clone(), commands.clone());
        preferences.connect_activate(move |_, _| {
            let Some(store) = w.store.clone() else { return };
            let w2 = w.clone();
            preferences::show(
                &window,
                store,
                commands.clone(),
                Rc::new(move |settings| {
                    w2.settings.replace(settings.clone());
                    if FAHRENHEIT.replace(settings.fahrenheit) != settings.fahrenheit {
                        load_week(&w2);
                        w2.history.refresh();
                        w2.sleep_page.refresh();
                        w2.activity_page.refresh();
                    }
                    redraw_temperature(&w2);
                    show_steps(&w2);
                    show_case(&w2);
                    show_sleep(&w2);
                }),
            );
        });
    }
    let about = gio::SimpleAction::new("about", None);
    {
        let window = window.clone();
        about.connect_activate(move |_, _| {
            let dialog = adw::AboutDialog::builder()
                .application_name("Ringmaster")
                .application_icon("net.dunevent.Ringmaster")
                .version(env!("CARGO_PKG_VERSION"))
                .comments("A companion for smart rings that keeps everything on this device. Not affiliated with any ring maker. Not a medical device.")
                .developer_name("Ringmaster contributors")
                .developers(["AJ Dunevent"])
                .copyright("© 2026 Ringmaster contributors")
                .license_type(gtk::License::Gpl30)
                .website("https://dev.dunevent.net/aj/Ringmaster")
                .issue_url("https://dev.dunevent.net/aj/Ringmaster/issues")
                .build();
            dialog.add_link("Donate", "https://dunevent.net/donate");
            dialog.present(Some(&window));
        });
    }
    let quit = gio::SimpleAction::new("quit", None);
    {
        let app = app.clone();
        quit.connect_activate(move |_, _| {
            let _ = commands.try_send(Command::Shutdown);
            // Gives the backend a moment to close the link cleanly.
            let app = app.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || app.quit());
        });
    }
    app.add_action(&preferences);
    app.add_action(&about);
    app.add_action(&quit);
    app.set_accels_for_action("app.quit", &["<Control>q"]);
    app.set_accels_for_action("app.preferences", &["<Control>comma"]);
}

pub(super) fn temperature_text(celsius: f32, fahrenheit: bool) -> String {
    if fahrenheit {
        format!("{:.1} °F", to_fahrenheit(celsius as f64))
    } else {
        format!("{celsius:.1} °C")
    }
}

/// Shows the newer of the last status frame and the last history record.
fn redraw_temperature(w: &Widgets) {
    let live = w.last_telemetry.borrow().as_ref().map(|(unix, d)| (*unix, d.skin_temp[0].max(d.skin_temp[1]) as f64));
    let newest = match (live, w.history_temperature.get()) {
        (Some(l), Some(h)) => Some(if h.0 > l.0 { h } else { l }),
        (l, h) => l.or(h),
    };
    if let Some((unix, celsius)) = newest {
        w.temperature.1.set_text(&temperature_text(celsius as f32, w.settings.borrow().fahrenheit));
        w.temperature.0.set_subtitle(&when(unix));
    }
}

fn steps_text(steps: i64, goal: u32) -> String {
    format!("{steps} / {goal}")
}

fn show_steps(w: &Widgets) {
    let (Some(store), Some(ring)) = (w.store.as_ref(), w.ring.borrow().clone()) else { return };
    let Ok((steps, source)) = store.steps_today(&ring) else { return };
    w.steps.1.set_text(&steps_text(steps, w.settings.borrow().step_goal));
    w.steps.0.set_subtitle(match source {
        StepSource::Ring => "Today",
        StepSource::Connected => "Today, counted while connected",
    });
}

fn battery_icon(percent: u8, charging: bool) -> String {
    let level = (percent.min(100) as u32 + 5) / 10 * 10;
    match (level, charging) {
        (100, true) => "battery-level-100-charged-symbolic".into(),
        (_, true) => format!("battery-level-{level}-charging-symbolic"),
        _ => format!("battery-level-{level}-symbolic"),
    }
}

fn battery_indicator() -> (gtk::Box, gtk::Image, gtk::Label) {
    let icon = gtk::Image::from_icon_name("battery-missing-symbolic");
    let label = gtk::Label::builder().css_classes(["numeric"]).build();
    let indicator = gtk::Box::builder().spacing(4).valign(gtk::Align::Center).build();
    indicator.append(&icon);
    indicator.append(&label);
    (indicator, icon, label)
}

fn set_battery(image: &gtk::Image, label: &gtk::Label, percent: u8, charging: bool) {
    let mut icon = battery_icon(percent, charging);
    if !gtk::IconTheme::for_display(&image.display()).has_icon(&icon) {
        icon = battery_icon(percent, false);
    }
    image.set_icon_name(Some(&icon));
    label.set_text(&format!("{percent} %"));
}

/// The newer of the latest history value and the latest quick reading, and whether it is the
/// quick reading.
fn newest(history: Option<Reading<i64>>, quick: Option<Spot>) -> (Option<Reading<i64>>, bool) {
    match (history, quick) {
        (h, Some(q)) if h.is_none_or(|h| q.unix > h.unix) => (Some(Reading { value: q.value.round() as i64, unix: q.unix }), true),
        (h, _) => (h, false),
    }
}

fn show_vital(slot: &(adw::ActionRow, gtk::Label), history: Option<Reading<i64>>, quick: Option<Spot>, unit: &str) {
    let (reading, is_quick) = newest(history, quick);
    show_reading(slot, reading, |v| format!("{v} {unit}"));
    if let (Some(r), true) = (reading, is_quick) {
        slot.0.set_subtitle(&format!("{} · quick reading", when(r.unix)));
    }
}

/// Switches between the rings stored here.
fn pick_ring(w: &Widgets, send: impl Fn(Command) + 'static) {
    let Some(store) = w.store.as_ref() else { return };
    let Some(window) = w.toasts.root().and_downcast::<gtk::Window>() else { return };
    let entries = store.rings().unwrap_or_default();
    let current = w.ring.borrow().clone();
    let dialog = adw::AlertDialog::builder()
        .heading("Choose a ring")
        .body("Ringmaster shows, syncs and connects to one ring at a time. Each keeps its own history.")
        .close_response("cancel")
        .build();
    let picked: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(current.clone()));
    let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    let p = picked.clone();
    for row in rings::radio_rows(&entries, current.as_deref(), move |a| {
        p.replace(Some(a));
    }) {
        list.append(&row);
    }
    dialog.set_extra_child(Some(&list));
    dialog.add_responses(&[("cancel", "Cancel"), ("switch", "Switch")]);
    dialog.set_response_appearance("switch", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("switch"));
    dialog.connect_response(None, move |_, response| {
        if response != "switch" {
            return;
        }
        if let Some(address) = picked.borrow().clone().filter(|a| Some(a) != current.as_ref()) {
            send(Command::SwitchRing(address));
        }
    });
    dialog.present(Some(&window));
}

fn choose_ring(w: &Widgets, choices: Vec<RingChoice>, send: impl Fn(Command) + 'static) {
    let Some(window) = w.toasts.root().and_downcast::<gtk::Window>() else { return };
    let (heading, body) = match choices.len() {
        1 => ("New ring found", "Pair it with Ringmaster? It becomes the ring shown, synced and connected."),
        _ => ("Several rings nearby", "Choose yours. The strongest signal is usually the ring closest to this device."),
    };
    let dialog = adw::AlertDialog::builder().heading(heading).body(body).close_response("cancel").build();
    let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    let picked: Rc<RefCell<Option<String>>> = Default::default();
    let mut first: Option<gtk::CheckButton> = None;
    for (i, c) in choices.iter().enumerate() {
        let check = gtk::CheckButton::new();
        if let Some(f) = &first {
            check.set_group(Some(f));
        }
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&c.name))
            .subtitle(format!("{} · {} dBm{}", c.address, c.rssi, if i == 0 { " · strongest" } else { "" }))
            .activatable_widget(&check)
            .build();
        row.add_prefix(&check);
        let (picked, address) = (picked.clone(), c.address.clone());
        check.connect_toggled(move |b| {
            if b.is_active() {
                picked.replace(Some(address.clone()));
            }
        });
        if i == 0 {
            check.set_active(true);
            first = Some(check);
        }
        list.append(&row);
    }
    dialog.set_extra_child(Some(&list));
    dialog.add_responses(&[("cancel", "Cancel"), ("pair", "Pair")]);
    dialog.set_response_appearance("pair", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("pair"));
    dialog.connect_response(None, move |_, response| {
        if response == "pair" {
            if let Some(address) = picked.borrow().clone() {
                send(Command::ConnectTo(address));
            }
        }
    });
    dialog.present(Some(&window));
}

/// Shown while the ring sits in its case and reports the case's battery, or with "Always" the
/// last reading the case sent, dated so its age is plain.
fn show_case(w: &Widgets) {
    let live = w
        .last_telemetry
        .borrow()
        .as_ref()
        .filter(|_| w.connected.get())
        .and_then(|(unix, d)| d.case_battery().map(|(p, plugged)| (*unix, p, plugged)));
    let case = live.or_else(|| w.stored_case.get().filter(|_| w.settings.borrow().case_always));
    w.case_row.set_visible(case.is_some());
    let stale = live.is_none() && case.is_some_and(|(unix, ..)| chrono::Utc::now().timestamp() - unix > CASE_STALE);
    if stale {
        w.case_row.add_css_class("stale");
    } else {
        w.case_row.remove_css_class("stale");
    }
    if let Some((unix, percent, plugged)) = case {
        // Whether the case is plugged in is only known while the ring sits in it.
        set_battery(&w.case_icon, &w.case_value, percent, plugged && live.is_some());
        let seen = if live.is_some() { "now".to_string() } else { when(unix) };
        w.case_row.set_subtitle(&format!("Last seen: {seen}"));
    }
}

fn spot_text(s: &Spot, kind: MeasureKind) -> String {
    match s.range {
        Some((low, high)) => format!("Low {low:.0} · high {high:.0} {}", unit(kind)),
        None => String::new(),
    }
}

fn selected_kind(m: &Measure) -> MeasureKind {
    if m.spo2.is_active() {
        MeasureKind::Spo2
    } else {
        MeasureKind::HeartRate
    }
}

fn slot(kind: MeasureKind) -> usize {
    match kind {
        MeasureKind::HeartRate => 0,
        MeasureKind::Spo2 => 1,
    }
}

/// Results live in Latest readings, so the sheet starts empty each time it opens and keeps only
/// what was measured while it stayed open.
fn show_session(m: &Measure) {
    if m.active.get().is_some() {
        return;
    }
    let shown = m.session.borrow()[slot(selected_kind(m))].clone();
    let (value, status) = shown.unwrap_or_else(|| ("–".into(), String::new()));
    m.value.set_text(&value);
    set_status(m, &status);
}

fn set_status(m: &Measure, text: &str) {
    m.status.set_text(text);
    m.status.set_visible(!text.is_empty() && m.active.get().is_none());
}

fn load_week(w: &Widgets) {
    let (Some(store), Some(ring)) = (w.store.as_ref(), w.ring.borrow().clone()) else { return };
    let span = w.week.span.get();
    let (from, to) = match span {
        Span::Week => {
            let today = Local::now().date_naive();
            (local_midnight(today - Days::days(6)), local_midnight(today + Days::days(1)))
        }
        Span::Day => {
            let end = next_hour();
            (end - 86_400, end)
        }
        Span::Hour => {
            let now = chrono::Utc::now().timestamp();
            (now - 3600, now)
        }
    };
    let metric = w.week.metric.get();
    let points = series(store, &ring, metric, from, to);
    let p = metric.precision();
    // History arrives at each sync, so the hour view says how recent its newest reading is.
    let latest = match (span, points.last()) {
        (Span::Hour, Some(&(t, _))) => format!(" · latest {}", clock(t)),
        _ => String::new(),
    };
    w.week.summary.set_text(
        &Stats::of(points.iter().map(|p| p.1))
            .map(|s| {
                format!(
                    "{} · average {:.*} {}, range {:.*} to {:.*}{latest}",
                    metric.name(),
                    p,
                    s.mean,
                    metric_unit(metric),
                    p,
                    s.min,
                    p,
                    s.max
                )
            })
            .unwrap_or_else(|| {
                let span = match span {
                    Span::Hour => "60 minutes",
                    Span::Day => "24 hours",
                    Span::Week => "7 days",
                };
                format!("No {} readings in the past {span}", metric.name().to_lowercase())
            }),
    );
    w.week.chart.set(metric, from, to, points, store.sleep(&ring, from, to).unwrap_or_default());
}

/// Hours to one decimal place, for example "6.5 h".
pub(super) fn duration(seconds: i64) -> String {
    format!("{:.1} h", seconds as f64 / 3600.0)
}

/// "8", or "7.5" for a goal with minutes; at most two decimal places, as entered in Preferences.
fn goal_text(seconds: i64) -> String {
    let text = format!("{:.2}", seconds as f64 / 3600.0);
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The case reports its battery only while the ring sits in it; older readings are dimmed.
const CASE_STALE: i64 = 12 * 3600;

/// The start of the next local hour.
pub(super) fn next_hour() -> i64 {
    let now = Local::now();
    let hour = now.with_minute(0).and_then(|t| t.with_second(0)).and_then(|t| t.with_nanosecond(0)).unwrap_or(now);
    (hour + Days::hours(1)).timestamp()
}

fn toast(w: &Widgets, text: &str) {
    let mut chars = text.chars();
    let text: String = chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default();
    w.toasts.add_toast(adw::Toast::builder().title(glib::markup_escape_text(&text)).timeout(5).build());
}

fn show_reading<T>(slot: &(adw::ActionRow, gtk::Label), reading: Option<Reading<T>>, format: impl Fn(T) -> String) {
    match reading {
        Some(r) => {
            slot.1.set_text(&format(r.value));
            slot.0.set_subtitle(&when(r.unix));
        }
        None => {
            slot.1.set_text("–");
            slot.0.set_subtitle("");
        }
    }
}

fn set_measuring(m: &Measure, kind: Option<MeasureKind>) {
    m.active.set(kind);
    m.heart_rate.set_sensitive(kind.is_none());
    m.spo2.set_sensitive(kind.is_none());
    m.progress.set_visible(kind.is_some());
    m.status.set_visible(kind.is_none() && !m.status.text().is_empty());
    m.button.set_label(if kind.is_some() { "Stop" } else { "Measure now" });
    m.button.set_tooltip_text(kind.is_none().then_some("Takes about 30 seconds"));
    m.button.set_css_classes(if kind.is_some() { &["pill", "destructive-action"] } else { &["pill", "suggested-action"] });
    if let Some(id) = m.animation.take() {
        id.remove();
    }
    m.locked.set(None);
    if kind.is_some() {
        // Bounces until the first reading, then fills smoothly over the reading window, since
        // readings arrive only every 2 s.
        let (progress, locked) = (m.progress.clone(), m.locked.clone());
        progress.set_pulse_step(0.08);
        let id = glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            match locked.get() {
                None => progress.pulse(),
                Some(t) => progress.set_fraction((t.elapsed().as_secs_f64() / crate::backend::MEASURE_WINDOW.as_secs_f64()).min(1.0)),
            }
            glib::ControlFlow::Continue
        });
        m.animation.replace(Some(id));
    }
}

fn stop_sync_pulse(w: &Widgets) {
    if let Some(id) = w.sync_pulse.take() {
        id.remove();
    }
}

fn apply(w: &Widgets, event: Event, send: &(impl Fn(Command) + Clone + 'static)) {
    match event {
        Event::Link(state) => {
            let (subtitle, connected, busy) = match &state {
                LinkState::Disconnected => ("Not connected".to_string(), false, false),
                LinkState::Searching => ("Searching…".into(), false, true),
                LinkState::Connecting => ("Connecting…".into(), false, true),
                LinkState::Pairing => ("Pairing…".into(), false, true),
                LinkState::Authenticating => ("Authenticating…".into(), false, true),
                LinkState::Connected { name } => {
                    w.ring_name.replace(Some(name.clone()));
                    ("Connected".into(), true, false)
                }
            };
            w.connected.set(connected);
            w.ring_row.set_subtitle(&subtitle);
            if let Some(name) = w.ring_name.borrow().as_ref() {
                w.ring_row.set_title(name);
            }
            w.connect.set_label(if connected { "Disconnect" } else { "Connect" });
            w.connect.set_css_classes(if connected { &[] } else { &["suggested-action"] });
            w.connect.set_sensitive(!busy);
            w.sync.set_sensitive(connected);
            w.measure.button.set_sensitive(connected);
            // Battery and Quick reading need a live link; a stored battery level would be stale.
            w.quick_row.set_sensitive(connected);
            // The next status frame shows it again once connected.
            if !connected {
                w.battery_box.set_visible(false);
            }
            show_case(w);
            if !connected {
                stop_sync_pulse(w);
                w.progress.set_visible(false);
                if w.measure.active.get().is_some() {
                    set_measuring(&w.measure, None);
                    set_status(&w.measure, "The ring disconnected");
                }
            }
        }
        Event::Telemetry(d) => show_telemetry(w, chrono::Utc::now().timestamp(), &d),
        Event::Battery { percent, charging } => {
            set_battery(&w.battery_icon, &w.battery, percent, charging);
            w.battery_box.set_visible(w.connected.get());
        }
        Event::SyncStarted => {
            // The sync bar takes over from the pull's spinner.
            w.pull.finish();
            w.sync.set_sensitive(false);
            w.progress_total.set(0);
            w.progress.set_fraction(0.0);
            w.progress.set_visible(true);
            stop_sync_pulse(w);
            let progress = w.progress.clone();
            progress.set_pulse_step(0.08);
            w.sync_pulse.replace(Some(glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
                progress.pulse();
                glib::ControlFlow::Continue
            })));
        }
        Event::SyncProgress { records, remaining } => {
            let total = (records as u32 + remaining as u32).max(w.progress_total.get());
            w.progress_total.set(total);
            if records > 0 {
                stop_sync_pulse(w);
                w.progress.set_fraction(records as f64 / total.max(1) as f64);
            }
        }
        Event::SyncFinished { error } => {
            w.pull.finish();
            stop_sync_pulse(w);
            w.progress.set_visible(false);
            w.sync.set_sensitive(w.connected.get());
            if let Some(e) = error {
                toast(w, &format!("Sync stopped: {e}"));
            }
        }
        Event::MeasureStarted(kind) => {
            set_measuring(&w.measure, Some(kind));
            w.measure.value.set_text("–");
        }
        Event::MeasureProgress { value, progress } => {
            let Some(kind) = w.measure.active.get() else { return };
            if let Some(v) = value {
                w.measure.value.set_text(&format!("{v} {}", unit(kind)));
                if w.measure.locked.get().is_none() {
                    w.measure.locked.set(Some(
                        std::time::Instant::now()
                            - std::time::Duration::from_secs_f64(progress * crate::backend::MEASURE_WINDOW.as_secs_f64()),
                    ));
                }
            }
        }
        Event::MeasureFinished { kind, result } => {
            set_measuring(&w.measure, None);
            let shown = match result {
                Some(s) => (format!("{:.0} {}", s.value, unit(kind)), spot_text(&s, kind)),
                None => ("–".into(), "No steady reading. Check the fit, warm your hand and try again.".into()),
            };
            if w.measure.open.get() {
                w.measure.session.borrow_mut()[slot(kind)] = Some(shown);
            }
            show_session(&w.measure);
        }
        Event::Summary(s) => show_summary(w, &s),
        Event::Notify(alert) => {
            let n = gio::Notification::new(&alert.title);
            n.set_body(Some(&alert.body));
            w.app.send_notification(Some(&format!("rule-{}", alert.rule_id)), &n);
        }
        Event::ChooseRing(choices) => {
            w.pull.finish();
            choose_ring(w, choices, send.clone());
        }
        Event::Error(e) => {
            w.pull.finish();
            w.connect.set_sensitive(true);
            toast(w, &e);
        }
    }
}

fn show_telemetry(w: &Widgets, unix: i64, d: &Descriptor) {
    let charging = d.ring_state() == RingState::Charging;
    set_battery(&w.battery_icon, &w.battery, d.battery, charging);
    w.battery_box.set_visible(w.connected.get());
    w.last_telemetry.replace(Some((unix, d.clone())));
    redraw_temperature(w);
    show_case(w);
    show_steps(w);
}

fn show_summary(w: &Widgets, s: &Summary) {
    if let Some(ring) = &s.ring {
        let name = ring.display_name();
        w.ring_row.set_title(&name);
        w.ring_name.replace(Some(name));
        let previous = w.ring.replace(Some(ring.address.clone()));
        // The last status frame belongs to the ring shown before a switch.
        if previous.as_ref().is_some_and(|p| *p != ring.address) {
            w.last_telemetry.replace(None);
            w.battery_box.set_visible(false);
        }
        if previous.as_ref() != Some(&ring.address) {
            w.history.open_latest();
        }
    }
    let several = w.store.as_ref().and_then(|st| st.rings().ok()).is_some_and(|r| r.len() > 1);
    w.ring_switch.set_visible(several);
    w.ring_row.set_activatable(several);
    w.sync_row.set_subtitle(&s.last_sync.map(when).unwrap_or_else(|| "Never".into()));
    show_vital(&w.heart_rate, s.heart_rate, s.spot_heart_rate, "bpm");
    show_reading(&w.hrv, s.hrv, |v| format!("{v} ms"));
    show_vital(&w.spo2, s.spo2, s.spot_spo2, "%");
    show_reading(&w.resp, s.resp_rate, |v| format!("{v:.1} breaths/min"));
    w.history_temperature.set(s.skin_temp.map(|r| (r.unix, r.value)));
    w.stored_case.set(s.case);
    show_case(w);
    redraw_temperature(w);
    if let Some((unix, d)) = &s.telemetry {
        if w.last_telemetry.borrow().as_ref().is_none_or(|(seen, _)| unix > seen) {
            show_telemetry(w, *unix, d);
        }
    }
    show_steps(w);
    show_sleep(w);
    load_week(w);
    w.history.refresh();
    w.sleep_page.refresh();
    w.activity_page.refresh();
}

/// "23:10–06:48 · 52 min awake": when the sleep ran and the wakings left out of its duration.
pub(super) fn sleep_subtitle(spans: &[(i64, i64)], awake: i64, at: impl Fn(i64) -> String) -> String {
    let mut text = spans.iter().map(|(a, b)| format!("{}–{}", at(*a), at(*b))).collect::<Vec<_>>().join(", ");
    if awake >= 60 {
        text.push_str(&format!(" · {} min awake", awake / 60));
    }
    text
}

/// The longest sleep that ended in the past day, so a later nap does not replace the night.
fn show_sleep(w: &Widgets) {
    let (Some(store), Some(ring)) = (w.store.as_ref(), w.ring.borrow().clone()) else { return };
    let now = chrono::Utc::now().timestamp();
    let last = store
        .sleep(&ring, now - 86_400, now)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, end)| *end > now - 86_400)
        .max_by_key(|(start, end)| end - start);
    let at = |u: i64| Local.timestamp_opt(u, 0).single().map(|t| t.format("%H:%M").to_string()).unwrap_or_default();
    w.last_night.set(last);
    w.sleep.0.set_activatable(last.is_some());
    match last {
        Some((start, end)) => {
            let awake = store.awake(&ring, start, end).unwrap_or(0);
            let goal = w.settings.borrow().sleep_goal_minutes as i64 * 60;
            w.sleep.1.set_text(&format!("{:.1} / {} h", (end - start - awake) as f64 / 3600.0, goal_text(goal)));
            w.sleep.0.set_subtitle(&sleep_subtitle(&[(start, end)], awake, at));
        }
        None => {
            w.sleep.1.set_text("–");
            w.sleep.0.set_subtitle("");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_against_goal() {
        assert_eq!(steps_text(6822, 10_000), "6822 / 10000");
        let history = Some(Reading { value: 61, unix: 100 });
        let quick = Some(Spot { value: 63.4, samples: 10, unix: 200, range: None });
        assert_eq!(newest(history, quick), (Some(Reading { value: 63, unix: 200 }), true));
        assert_eq!(newest(Some(Reading { value: 61, unix: 300 }), quick), (Some(Reading { value: 61, unix: 300 }), false));
        assert_eq!(battery_icon(64, false), "battery-level-60-symbolic");
        assert_eq!(battery_icon(100, true), "battery-level-100-charged-symbolic");
        assert_eq!(battery_icon(42, true), "battery-level-40-charging-symbolic");
    }
}
