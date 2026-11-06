use crate::backend::Command;
use crate::settings::{to_celsius, to_fahrenheit, Op, Rule, Settings, Stat, SLEEP_GOAL_RANGE, STEP_GOAL_RANGE, SYNC_INTERVALS};
use crate::store::{RingInfo, Store};
use adw::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Called after settings are saved, so the window can redraw in the new units.
pub type OnChange = Rc<dyn Fn(&Settings)>;

const NOTE: &str = "A notification is sent when its condition becomes true, and again only after the \
condition has cleared, at most once every 30 minutes. Heart rate, HRV, blood oxygen and respiratory \
rate arrive with each sync, so their notifications can come hours after the reading. Ringmaster is \
not a medical device.";

/// GLib's `%x` follows the locale's date format.
fn locale_date(unix: i64) -> String {
    glib::DateTime::from_unix_local(unix).and_then(|d| d.format("%x")).map(|s| s.to_string()).unwrap_or_default()
}

fn import_subtitle(store: &Store) -> String {
    match store.value(crate::import::OFFICIAL_IMPORT_KEY).ok().flatten().and_then(|v| v.parse::<i64>().ok()) {
        Some(when) => format!("Imported on {}", locale_date(when)),
        None => "Copies all history from the official Android app's ring_conn.db".into(),
    }
}

/// Hours, with a decimal point or comma, as minutes.
fn parse_sleep_goal(text: &str) -> Option<u32> {
    let hours: f64 = text.trim().replace(',', ".").parse().ok()?;
    let minutes = (hours * 60.0).round();
    (minutes.is_finite() && minutes >= 0.0).then_some(minutes as u32).filter(|m| SLEEP_GOAL_RANGE.contains(m))
}

fn sleep_goal_text(minutes: u32) -> String {
    let hours = minutes as f64 / 60.0;
    if minutes.is_multiple_of(60) {
        format!("{hours:.0}")
    } else {
        format!("{hours:.2}").trim_end_matches('0').to_string()
    }
}

fn parse_step_goal(text: &str) -> Option<u32> {
    text.trim().replace([',', '.', ' '], "").parse().ok().filter(|g| STEP_GOAL_RANGE.contains(g))
}

fn interval_label(minutes: Option<u32>) -> String {
    match minutes {
        None => "Never".into(),
        Some(60) => "Every hour".into(),
        Some(m) => format!("Every {m} minutes"),
    }
}

/// Accepted range and shown decimals for a stat, in the unit shown.
fn range(stat: Stat, fahrenheit: bool) -> (f64, f64, usize) {
    match stat {
        Stat::Battery | Stat::CaseBattery => (0.0, 100.0, 0),
        Stat::SkinTemp if fahrenheit => (68.0, 113.0, 1),
        Stat::SkinTemp => (20.0, 45.0, 1),
        Stat::HeartRate => (20.0, 250.0, 0),
        Stat::Hrv => (0.0, 300.0, 0),
        Stat::Spo2 => (50.0, 100.0, 0),
        Stat::RespRate => (0.0, 60.0, 1),
        Stat::DailySteps | Stat::StepsToGoal => (0.0, 100_000.0, 0),
        // The ring keeps about 7 days of history.
        Stat::DaysSinceSync => (1.0, 7.0, 0),
    }
}

/// Starting threshold when a rule switches to a stat its old value does not fit.
fn default_threshold(stat: Stat, fahrenheit: bool) -> f64 {
    match stat {
        Stat::Battery => 5.0,
        Stat::CaseBattery => 10.0,
        Stat::SkinTemp => shown(stat, 38.0, fahrenheit),
        Stat::HeartRate => 120.0,
        Stat::Hrv => 20.0,
        Stat::Spo2 => 90.0,
        Stat::RespRate => 25.0,
        Stat::DailySteps => 10_000.0,
        Stat::StepsToGoal => 0.0,
        Stat::DaysSinceSync => 5.0,
    }
}

/// The entry's value if it parses and lies in the stat's range.
fn parse_value(text: &str, stat: Stat, fahrenheit: bool) -> Option<f64> {
    let (lo, hi, _) = range(stat, fahrenheit);
    text.trim().replace(',', ".").parse::<f64>().ok().filter(|v| (lo..=hi).contains(v))
}

fn shown(stat: Stat, stored: f64, fahrenheit: bool) -> f64 {
    if stat == Stat::SkinTemp && fahrenheit {
        to_fahrenheit(stored)
    } else {
        stored
    }
}

fn stored(stat: Stat, shown: f64, fahrenheit: bool) -> f64 {
    if stat == Stat::SkinTemp && fahrenheit {
        to_celsius(shown)
    } else {
        shown
    }
}

struct Ctx {
    store: Rc<Store>,
    commands: async_channel::Sender<Command>,
    dialog: adw::PreferencesDialog,
    settings: RefCell<Settings>,
    on_change: OnChange,
    rules_group: adw::PreferencesGroup,
    rule_rows: RefCell<Vec<adw::PreferencesRow>>,
    add_row: adw::ActionRow,
}

impl Ctx {
    fn save(&self) {
        let settings = self.settings.borrow().clone();
        if let Err(e) = self.store.save_settings(&settings) {
            self.toast(&format!("Saving settings failed: {e:#}"));
        }
        let _ = self.commands.try_send(Command::ReloadSettings);
        (self.on_change)(&settings);
    }

    fn toast(&self, text: &str) {
        self.dialog.add_toast(adw::Toast::new(text));
    }

    fn rules_changed(&self) {
        let _ = self.commands.try_send(Command::ReloadSettings);
    }
}

pub fn show(parent: &impl IsA<gtk::Widget>, store: Rc<Store>, commands: async_channel::Sender<Command>, on_change: OnChange) {
    let dialog = adw::PreferencesDialog::new();
    let settings = store.settings().unwrap_or_default();
    let ctx = Rc::new(Ctx {
        store,
        commands,
        dialog: dialog.clone(),
        settings: RefCell::new(settings.clone()),
        on_change,
        rules_group: adw::PreferencesGroup::builder().title("Notifications").description(NOTE).build(),
        rule_rows: Default::default(),
        add_row: adw::ActionRow::builder().title("Add notification").activatable(true).build(),
    });

    let general = adw::PreferencesPage::builder().title("General").icon_name("emblem-system-symbolic").build();

    let connection = adw::PreferencesGroup::builder().title("Connection").build();
    let stay = adw::SwitchRow::builder()
        .title("Stay connected")
        .subtitle("Keeps the link open for live battery and case status, and reconnects when it drops")
        .active(settings.stay_connected)
        .build();
    connection.add(&stay);

    let sync = adw::PreferencesGroup::builder().title("Sync").build();
    let on_open = adw::SwitchRow::builder().title("Sync when the app opens").active(settings.sync_on_open).build();
    let labels: Vec<String> = SYNC_INTERVALS.iter().map(|h| interval_label(*h)).collect();
    let interval = adw::ComboRow::builder()
        .title("Automatic sync")
        .model(&gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>()))
        .selected(SYNC_INTERVALS.iter().position(|m| *m == settings.sync_interval_minutes).unwrap_or(2) as u32)
        .build();
    sync.add(&on_open);
    sync.add(&interval);

    let background = adw::PreferencesGroup::builder().title("Background").build();
    let run_bg = adw::SwitchRow::builder().title("Run in background").active(settings.run_in_background).build();
    let login = adw::SwitchRow::builder().title("Start at login").active(settings.start_at_login).build();
    background.add(&run_bg);
    background.add(&login);

    let units = adw::PreferencesGroup::builder().title("Units").build();
    let temperature = adw::ComboRow::builder()
        .title("Temperature")
        .model(&gtk::StringList::new(&["Celsius (°C)", "Fahrenheit (°F)"]))
        .selected(settings.fahrenheit as u32)
        .build();
    units.add(&temperature);

    let goals = adw::PreferencesGroup::builder().title("Goals").build();
    let step_goal = adw::EntryRow::builder()
        .title("Daily steps")
        .text(settings.step_goal.to_string())
        .input_purpose(gtk::InputPurpose::Digits)
        .show_apply_button(true)
        .build();
    step_goal.add_prefix(&super::dots::dot(super::dots::STEPS));
    goals.add(&step_goal);
    let sleep_goal = adw::EntryRow::builder()
        .title("Sleep duration (hrs)")
        .text(sleep_goal_text(settings.sleep_goal_minutes))
        .input_purpose(gtk::InputPurpose::Number)
        .show_apply_button(true)
        .build();
    sleep_goal.add_prefix(&super::dots::dot("metric-sleep"));
    goals.add(&sleep_goal);

    let hardware = adw::PreferencesPage::builder().title("Rings").icon_name("bluetooth-active-symbolic").build();
    let entries = ctx.store.rings().unwrap_or_default();
    let rings = adw::PreferencesGroup::builder().title("Rings").build();
    let add_ring = adw::ActionRow::builder().title("Add a ring…").activatable(true).build();
    add_ring.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));

    let ring = adw::PreferencesGroup::builder().title("Ring").build();
    let known = ctx.store.default_ring().ok().flatten().and_then(|a| ctx.store.ring(&a).ok().flatten());
    let selected: Rc<RefCell<Option<RingInfo>>> = Rc::new(RefCell::new(known.clone()));
    let name = adw::EntryRow::builder().show_apply_button(true).build();
    ring.add(&name);
    let case = adw::ComboRow::builder()
        .title("Charging case battery")
        .model(&gtk::StringList::new(&["Only when charging", "Always"]))
        .selected(settings.case_always as u32)
        .build();
    ring.add(&case);
    let import_button = gtk::Button::builder().label("Import…").valign(gtk::Align::Center).build();
    let import = adw::ActionRow::builder().title("Import from the RingConn app").subtitle(import_subtitle(&ctx.store)).build();
    import.add_suffix(&import_button);
    ring.add(&import);
    let forget_button = gtk::Button::builder().label("Forget").valign(gtk::Align::Center).css_classes(["destructive-action"]).build();
    let forget = adw::ActionRow::builder().title("Forget this ring").subtitle("Removes the pairing; its history stays").build();
    forget.add_suffix(&forget_button);
    ring.add(&forget);

    // Fills the Ring group for the selected ring.
    let show_ring = {
        let (ctx, selected, ring, name, import, forget) =
            (ctx.clone(), selected.clone(), ring.clone(), name.clone(), import.clone(), forget.clone());
        let several = entries.len() > 1;
        move || {
            let info = selected.borrow().clone();
            let forgotten = info.as_ref().is_some_and(|r| ctx.store.is_forgotten(&r.address).unwrap_or(false));
            ring.set_title(&info.as_ref().filter(|_| several).map_or_else(|| "Ring".into(), |r| r.display_name()));
            // The ring's own name shows as the title, and as the placeholder while the field is empty.
            name.set_title(&info.as_ref().and_then(|r| r.name.clone()).unwrap_or_else(|| "Name".into()));
            name.set_text(&info.as_ref().and_then(|r| r.nickname.clone()).unwrap_or_default());
            name.set_visible(info.is_some());
            // The import reads the RingConn app's database.
            import.set_visible(info.as_ref().is_some_and(|r| r.brand.as_deref() != Some(crate::store::COLMI)));
            forget.set_visible(info.is_some() && !forgotten);
        }
    };
    show_ring();
    {
        let (ctx, selected, show_ring) = (ctx.clone(), selected.clone(), show_ring.clone());
        let current = known.as_ref().map(|r| r.address.clone());
        for row in super::rings::radio_rows(&entries, current.as_deref(), move |address| {
            selected.replace(ctx.store.ring(&address).ok().flatten());
            show_ring();
            let _ = ctx.commands.try_send(Command::SwitchRing(address));
        }) {
            rings.add(&row);
        }
    }
    rings.add(&add_ring);

    hardware.add(&rings);
    hardware.add(&ring);
    for group in [&connection, &sync, &background, &units, &goals] {
        general.add(group);
    }

    let alerts = adw::PreferencesPage::builder().title("Notifications").icon_name("alarm-symbolic").build();
    ctx.add_row.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
    alerts.add(&ctx.rules_group);
    rebuild_rules(&ctx);

    for page in [&hardware, &general, &alerts] {
        dialog.add(page);
    }

    {
        let ctx = ctx.clone();
        stay.connect_active_notify(move |r| {
            ctx.settings.borrow_mut().stay_connected = r.is_active();
            ctx.save();
        });
    }
    {
        let ctx = ctx.clone();
        on_open.connect_active_notify(move |r| {
            ctx.settings.borrow_mut().sync_on_open = r.is_active();
            ctx.save();
        });
    }
    {
        let ctx = ctx.clone();
        interval.connect_selected_notify(move |r| {
            ctx.settings.borrow_mut().sync_interval_minutes = SYNC_INTERVALS.get(r.selected() as usize).copied().flatten();
            ctx.save();
        });
    }
    {
        let ctx = ctx.clone();
        run_bg.connect_active_notify(move |r| {
            ctx.settings.borrow_mut().run_in_background = r.is_active();
            ctx.save();
        });
    }
    {
        let ctx = ctx.clone();
        let reverting = Rc::new(Cell::new(false));
        login.connect_active_notify(move |r| {
            if reverting.get() {
                return;
            }
            let enabled = r.is_active();
            let (ctx, row, reverting) = (ctx.clone(), r.clone(), reverting.clone());
            glib::spawn_future_local(async move {
                match crate::autostart::set(enabled).await {
                    Ok(()) => {
                        ctx.settings.borrow_mut().start_at_login = enabled;
                        ctx.save();
                    }
                    Err(e) => {
                        reverting.set(true);
                        row.set_active(!enabled);
                        reverting.set(false);
                        ctx.toast(&format!("{e:#}"));
                    }
                }
            });
        });
    }
    {
        let ctx = ctx.clone();
        case.connect_selected_notify(move |r| {
            ctx.settings.borrow_mut().case_always = r.selected() == 1;
            ctx.save();
        });
    }
    {
        let ctx = ctx.clone();
        temperature.connect_selected_notify(move |r| {
            ctx.settings.borrow_mut().fahrenheit = r.selected() == 1;
            ctx.save();
            rebuild_rules(&ctx);
        });
    }
    {
        let ctx = ctx.clone();
        step_goal.connect_apply(move |row| match parse_step_goal(&row.text()) {
            Some(goal) => {
                row.remove_css_class("error");
                ctx.settings.borrow_mut().step_goal = goal;
                ctx.save();
            }
            None => {
                row.add_css_class("error");
                ctx.toast(&format!("Enter a whole number from {} to {}", STEP_GOAL_RANGE.start(), STEP_GOAL_RANGE.end()));
            }
        });
    }
    {
        let ctx = ctx.clone();
        sleep_goal.connect_apply(move |row| match parse_sleep_goal(&row.text()) {
            Some(goal) => {
                row.remove_css_class("error");
                ctx.settings.borrow_mut().sleep_goal_minutes = goal;
                ctx.save();
            }
            None => {
                row.add_css_class("error");
                ctx.toast(&format!("Enter {} to {} hours", SLEEP_GOAL_RANGE.start() / 60, SLEEP_GOAL_RANGE.end() / 60));
            }
        });
    }
    {
        let (ctx, selected) = (ctx.clone(), selected.clone());
        import_button.connect_clicked(move |button| {
            if let Some(address) = selected.borrow().as_ref().map(|r| r.address.clone()) {
                choose_import(&ctx, button, &import, address);
            }
        });
    }
    {
        let (ctx, selected) = (ctx.clone(), selected.clone());
        name.connect_apply(move |row| {
            let Some(address) = selected.borrow().as_ref().map(|r| r.address.clone()) else { return };
            match ctx.store.set_nickname(&address, &row.text()) {
                Ok(()) => ctx.rules_changed(),
                Err(e) => ctx.toast(&format!("Renaming failed: {e:#}")),
            }
        });
    }
    {
        let ctx = ctx.clone();
        add_ring.connect_activated(move |_| {
            let _ = ctx.commands.try_send(Command::AddRing);
            ctx.dialog.close();
        });
    }
    {
        let ctx = ctx.clone();
        forget_button.connect_clicked(move |_| {
            let confirm = adw::AlertDialog::new(
                Some("Forget this ring?"),
                Some("Ringmaster removes the pairing and stops connecting. The history stays. To use the ring again, seat it in its case for a moment, take it out and press Connect."),
            );
            confirm.add_responses(&[("cancel", "Cancel"), ("forget", "Forget")]);
            confirm.set_response_appearance("forget", adw::ResponseAppearance::Destructive);
            let ctx2 = ctx.clone();
            confirm.connect_response(None, move |_, response| {
                if response == "forget" {
                    let _ = ctx2.commands.try_send(Command::ForgetRing);
                    ctx2.dialog.close();
                }
            });
            confirm.present(Some(&ctx.dialog));
        });
    }
    {
        let ctx = ctx.clone();
        ctx.add_row.clone().connect_activated(move |_| {
            let fahrenheit = ctx.settings.borrow().fahrenheit;
            let row = rule_row(&ctx, None, fahrenheit);
            ctx.rules_group.remove(&ctx.add_row);
            ctx.rules_group.add(&row);
            ctx.rules_group.add(&ctx.add_row);
            ctx.rule_rows.borrow_mut().push(row);
        });
    }
    {
        // Leaving the page drops notifications that were never completed.
        let ctx = ctx.clone();
        dialog.connect_visible_page_notify(move |_| rebuild_rules(&ctx));
    }

    dialog.present(Some(parent));
}

/// Asks for the official app's database and copies it in on a worker thread.
fn choose_import(ctx: &Rc<Ctx>, button: &gtk::Button, row: &adw::ActionRow, ring: String) {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("SQLite database"));
    filter.add_pattern("*.db");
    filter.add_pattern("ring_conn*");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let dialog = gtk::FileDialog::builder().title("Choose ring_conn.db").modal(true).filters(&filters).default_filter(&filter).build();
    let parent = ctx.dialog.root().and_downcast::<gtk::Window>();
    let (ctx, button, row) = (ctx.clone(), button.clone(), row.clone());
    glib::spawn_future_local(async move {
        let Ok(file) = dialog.open_future(parent.as_ref()).await else { return };
        let Some(path) = file.path() else {
            ctx.toast("Choose a file stored on this device");
            return;
        };
        button.set_sensitive(false);
        row.set_subtitle("Importing…");
        let database = crate::database_path();
        let result = gio::spawn_blocking(move || {
            let mut store = Store::open(&database)?;
            crate::import::official_app(&mut store, &ring, &path)
        })
        .await;
        button.set_sensitive(true);
        row.set_subtitle(&import_subtitle(&ctx.store));
        match result {
            Ok(Ok(s)) => {
                let span = match (s.first, s.last) {
                    (Some(a), Some(b)) => format!(" from {} to {}", locale_date(a), locale_date(b)),
                    _ => String::new(),
                };
                ctx.toast(&format!("Imported {} readings{span}; {} were new", s.epochs, s.new_epochs));
                let _ = ctx.commands.try_send(Command::Refresh);
            }
            Ok(Err(e)) => ctx.toast(&format!("Import failed: {e:#}")),
            Err(_) => ctx.toast("Import failed"),
        }
    });
}

fn rebuild_rules(ctx: &Rc<Ctx>) {
    for row in ctx.rule_rows.take() {
        ctx.rules_group.remove(&row);
    }
    ctx.rules_group.remove(&ctx.add_row);
    let fahrenheit = ctx.settings.borrow().fahrenheit;
    let mut rows = Vec::new();
    for rule in ctx.store.rules().unwrap_or_default() {
        let row = rule_row(ctx, Some(rule), fahrenheit);
        ctx.rules_group.add(&row);
        rows.push(row);
    }
    ctx.rules_group.add(&ctx.add_row);
    ctx.rule_rows.replace(rows);
}

/// Preference groups place only `AdwPreferencesRow` children in their list. `None` starts an empty
/// row that is stored only once all three fields are valid.
fn rule_row(ctx: &Rc<Ctx>, rule: Option<Rule>, fahrenheit: bool) -> adw::PreferencesRow {
    let none = gtk::INVALID_LIST_POSITION;
    let stat = super::dots::dropdown(Stat::ALL.iter().map(|s| (super::dots::stat(*s), s.label(fahrenheit).to_string())).collect());
    stat.set_selected(rule.and_then(|r| Stat::ALL.iter().position(|s| *s == r.stat)).map_or(none, |p| p as u32));
    stat.set_hexpand(true);
    let op_labels: Vec<&str> = Op::ALL.iter().map(|o| o.symbol()).collect();
    let op = gtk::DropDown::from_strings(&op_labels);
    op.set_selected(rule.and_then(|r| Op::ALL.iter().position(|o| *o == r.op)).map_or(none, |p| p as u32));
    // A spin button changes value when scrolled over, which happens while scrolling the page with
    // a mouse wheel.
    let text = rule.map(|r| format!("{:.*}", range(r.stat, fahrenheit).2, shown(r.stat, r.threshold, fahrenheit))).unwrap_or_default();
    let value = gtk::Entry::builder().text(text).placeholder_text("Value").input_purpose(gtk::InputPurpose::Number).hexpand(true).build();
    let delete = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Delete")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    top.append(&stat);
    top.append(&delete);
    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bottom.append(&op);
    bottom.append(&value);
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(6)
        .build();
    content.append(&top);
    content.append(&bottom);
    let title = rule.map_or("New notification", |r| r.stat.label(fahrenheit));
    let row = adw::PreferencesRow::builder().title(title).child(&content).activatable(false).selectable(false).build();

    let id = Rc::new(Cell::new(rule.map(|r| r.id)));
    let save = {
        let (ctx, stat, op, value, id) = (ctx.clone(), stat.clone(), op.clone(), value.clone(), id.clone());
        Rc::new(move || {
            let (Some(&s), Some(&o)) = (Stat::ALL.get(stat.selected() as usize), Op::ALL.get(op.selected() as usize)) else { return };
            let Some(v) = parse_value(&value.text(), s, fahrenheit) else {
                if !value.text().is_empty() {
                    value.add_css_class("error");
                }
                return;
            };
            value.remove_css_class("error");
            let threshold = stored(s, v, fahrenheit);
            let result = match id.get() {
                Some(rule_id) => ctx.store.update_rule(&Rule { id: rule_id, stat: s, op: o, threshold }),
                None => ctx.store.add_rule(s, o, threshold).map(|new| id.set(Some(new))),
            };
            if let Err(e) = result {
                ctx.toast(&format!("Saving the notification failed: {e:#}"));
            }
            ctx.rules_changed();
        })
    };
    {
        let (save, value, id) = (save.clone(), value.clone(), id.clone());
        stat.connect_selected_notify(move |d| {
            let Some(&s) = Stat::ALL.get(d.selected() as usize) else { return };
            // A stored rule keeps a valid value; a new one waits for the user to type one.
            if id.get().is_some() && parse_value(&value.text(), s, fahrenheit).is_none() {
                // Setting the text saves through the entry's changed handler.
                value.set_text(&format!("{:.*}", range(s, fahrenheit).2, default_threshold(s, fahrenheit)));
            } else {
                save();
            }
        });
    }
    {
        let save = save.clone();
        op.connect_selected_notify(move |_| save());
    }
    value.connect_changed(move |_| save());
    {
        let (ctx, row, id) = (ctx.clone(), row.clone(), id.clone());
        delete.connect_clicked(move |_| {
            if let Some(rule_id) = id.get() {
                if let Err(e) = ctx.store.delete_rule(rule_id) {
                    ctx.toast(&format!("Deleting the notification failed: {e:#}"));
                }
                ctx.rules_changed();
            }
            ctx.rules_group.remove(&row);
            ctx.rule_rows.borrow_mut().retain(|r| r != &row);
        });
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_outside_the_range_are_refused() {
        assert_eq!(parse_value(" 5 ", Stat::Battery, false), Some(5.0));
        assert_eq!(parse_value("101", Stat::Battery, false), None);
        assert_eq!(parse_value("98,6", Stat::SkinTemp, true), Some(98.6));
        assert_eq!(parse_value("98.6", Stat::SkinTemp, false), None);
        assert_eq!(parse_value("abc", Stat::HeartRate, false), None);
    }

    #[test]
    fn sleep_goals() {
        assert_eq!(parse_sleep_goal("8"), Some(480));
        assert_eq!(parse_sleep_goal(" 7,5 "), Some(450));
        assert_eq!(parse_sleep_goal("2"), None);
        assert_eq!(parse_sleep_goal("eight"), None);
        assert_eq!((sleep_goal_text(480), sleep_goal_text(450), sleep_goal_text(465)), ("8".into(), "7.5".into(), "7.75".into()));
    }

    #[test]
    fn step_goals() {
        assert_eq!(parse_step_goal(" 10,000 "), Some(10_000));
        assert_eq!(parse_step_goal("50"), None);
        assert_eq!(parse_step_goal("ten"), None);
    }

    #[test]
    fn defaults_fit_their_ranges() {
        for f in [false, true] {
            for stat in Stat::ALL {
                let (lo, hi, _) = range(stat, f);
                assert!((lo..=hi).contains(&default_threshold(stat, f)), "{stat:?}");
            }
        }
    }
}
