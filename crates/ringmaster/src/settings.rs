//! User settings and notification rules, and the logic that decides when a rule fires.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Keep a connection open and reconnect when it drops.
    pub stay_connected: bool,
    pub sync_on_open: bool,
    /// `None` for never.
    pub sync_interval_minutes: Option<u32>,
    pub run_in_background: bool,
    pub start_at_login: bool,
    pub fahrenheit: bool,
    pub step_goal: u32,
    pub sleep_goal_minutes: u32,
    /// Show the charging case's last known battery even when the ring is out of the case.
    pub case_always: bool,
    pub profile: Profile,
    /// Height in inches and weight in lb, rather than cm and kg.
    pub us_body_units: bool,
}

/// Optional details that pick the published figures readings are compared with. Each is
/// `None` until set; calculations then use figures for adults in general.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Profile {
    pub birth_year: Option<u16>,
    pub reference: Reference,
    pub height_cm: Option<f64>,
    pub weight_kg: Option<f64>,
}

/// Which published reference ranges readings are compared with. It only chooses figures:
/// no wording or anything else in the app depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Reference {
    #[default]
    AllAdults,
    Female,
    Male,
}

impl Reference {
    pub const ALL: [Reference; 3] = [Reference::AllAdults, Reference::Female, Reference::Male];

    fn key(self) -> &'static str {
        match self {
            Reference::AllAdults => "all",
            Reference::Female => "female",
            Reference::Male => "male",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Reference::AllAdults => "All adults",
            Reference::Female => "Female ranges",
            Reference::Male => "Male ranges",
        }
    }
}

pub const BIRTH_YEAR_FIRST: u16 = 1900;
pub const HEIGHT_RANGE_CM: std::ops::RangeInclusive<f64> = 50.0..=250.0;
pub const WEIGHT_RANGE_KG: std::ops::RangeInclusive<f64> = 20.0..=350.0;
const CM_PER_INCH: f64 = 2.54;
const KG_PER_POUND: f64 = 0.453_592_37;

/// A birth year from `text`: `Ok(None)` for empty text, `Err` for anything outside
/// `BIRTH_YEAR_FIRST` to `this_year`.
pub fn parse_birth_year(text: &str, this_year: u16) -> Result<Option<u16>, ()> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse().ok().filter(|y| (BIRTH_YEAR_FIRST..=this_year).contains(y)).map(Some).ok_or(())
}

/// A height in cm from `text`, read as inches with US units.
pub fn parse_height(text: &str, us: bool) -> Result<Option<f64>, ()> {
    let text = text.trim().trim_end_matches(|c: char| c.is_alphabetic() || c == ' ' || c == '"');
    if text.is_empty() {
        return Ok(None);
    }
    let n: f64 = text.replace(',', ".").parse().map_err(|_| ())?;
    let cm = if us { n * CM_PER_INCH } else { n };
    HEIGHT_RANGE_CM.contains(&cm).then_some(Some(cm)).ok_or(())
}

pub fn height_text(cm: f64, us: bool) -> String {
    if us {
        format!("{:.0}", cm / CM_PER_INCH)
    } else {
        format!("{cm:.0}")
    }
}

/// A weight in kg from `text`, read as lb with US units.
pub fn parse_weight(text: &str, us: bool) -> Result<Option<f64>, ()> {
    let text = text.trim().trim_end_matches(|c: char| c.is_alphabetic() || c == ' ');
    if text.is_empty() {
        return Ok(None);
    }
    let n: f64 = text.replace(',', ".").parse().map_err(|_| ())?;
    let kg = if us { n * KG_PER_POUND } else { n };
    WEIGHT_RANGE_KG.contains(&kg).then_some(Some(kg)).ok_or(())
}

pub fn weight_text(kg: f64, us: bool) -> String {
    if us {
        format!("{:.0}", kg / KG_PER_POUND)
    } else {
        format!("{kg:.1}").trim_end_matches(".0").to_string()
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            stay_connected: true,
            sync_on_open: true,
            sync_interval_minutes: Some(15),
            run_in_background: true,
            start_at_login: false,
            fahrenheit: false,
            step_goal: 10_000,
            sleep_goal_minutes: 8 * 60,
            case_always: false,
            profile: Profile::default(),
            us_body_units: false,
        }
    }
}

pub const STEP_GOAL_RANGE: std::ops::RangeInclusive<u32> = 100..=100_000;
/// In minutes: 3 to 14 hours.
pub const SLEEP_GOAL_RANGE: std::ops::RangeInclusive<u32> = 180..=840;

/// In minutes. The official app syncs every 15 minutes.
pub const SYNC_INTERVALS: [Option<u32>; 6] = [None, Some(5), Some(15), Some(30), Some(60), Some(90)];

/// The offered interval closest to `minutes`; saved values from longer lists become the longest.
fn offered_interval(minutes: u32) -> u32 {
    SYNC_INTERVALS.iter().flatten().copied().min_by_key(|m| m.abs_diff(minutes)).unwrap_or(15)
}

fn flag(v: bool) -> String {
    if v { "1" } else { "0" }.into()
}

impl Settings {
    pub fn to_pairs(&self) -> Vec<(&'static str, String)> {
        vec![
            ("stay_connected", flag(self.stay_connected)),
            ("sync_on_open", flag(self.sync_on_open)),
            ("sync_interval_minutes", self.sync_interval_minutes.map_or("never".into(), |m| m.to_string())),
            ("run_in_background", flag(self.run_in_background)),
            ("start_at_login", flag(self.start_at_login)),
            ("fahrenheit", flag(self.fahrenheit)),
            ("step_goal", self.step_goal.to_string()),
            ("sleep_goal_minutes", self.sleep_goal_minutes.to_string()),
            ("case_battery_always", flag(self.case_always)),
            ("birth_year", self.profile.birth_year.map_or(String::new(), |y| y.to_string())),
            ("reference_ranges", self.profile.reference.key().into()),
            ("height_cm", self.profile.height_cm.map_or(String::new(), |v| format!("{v:.1}"))),
            ("weight_kg", self.profile.weight_kg.map_or(String::new(), |v| format!("{v:.2}"))),
            ("us_body_units", flag(self.us_body_units)),
        ]
    }

    pub fn from_pairs(pairs: &HashMap<String, String>) -> Self {
        let mut s = Self::default();
        let get = |k: &str| pairs.get(k).map(String::as_str);
        let b = |k: &str, d: bool| get(k).map_or(d, |v| v == "1");
        s.stay_connected = b("stay_connected", s.stay_connected);
        s.sync_on_open = b("sync_on_open", s.sync_on_open);
        s.run_in_background = b("run_in_background", s.run_in_background);
        s.start_at_login = b("start_at_login", s.start_at_login);
        s.fahrenheit = b("fahrenheit", s.fahrenheit);
        s.case_always = b("case_battery_always", s.case_always);
        s.us_body_units = b("us_body_units", s.us_body_units);
        s.profile.birth_year = get("birth_year").and_then(|v| v.parse().ok()).filter(|y| *y >= BIRTH_YEAR_FIRST);
        s.profile.reference = get("reference_ranges").and_then(|v| Reference::ALL.into_iter().find(|r| r.key() == v)).unwrap_or_default();
        s.profile.height_cm = get("height_cm").and_then(|v| v.parse().ok()).filter(|v| HEIGHT_RANGE_CM.contains(v));
        s.profile.weight_kg = get("weight_kg").and_then(|v| v.parse().ok()).filter(|v| WEIGHT_RANGE_KG.contains(v));
        if let Some(goal) = get("step_goal").and_then(|v| v.parse().ok()).filter(|g| STEP_GOAL_RANGE.contains(g)) {
            s.step_goal = goal;
        }
        if let Some(goal) = get("sleep_goal_minutes").and_then(|v| v.parse().ok()).filter(|g| SLEEP_GOAL_RANGE.contains(g)) {
            s.sleep_goal_minutes = goal;
        }
        if let Some(v) = get("sync_interval_minutes") {
            s.sync_interval_minutes = v.parse().ok().map(offered_interval);
        } else if let Some(v) = get("sync_interval_hours") {
            // Saved by versions that counted in hours.
            s.sync_interval_minutes = v.parse::<u32>().ok().map(|h| offered_interval(h * 60));
        }
        s
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stat {
    Battery,
    CaseBattery,
    SkinTemp,
    HeartRate,
    Hrv,
    Spo2,
    RespRate,
    DailySteps,
    StepsToGoal,
    DaysSinceSync,
}

impl Stat {
    pub const ALL: [Stat; 10] = [
        Stat::Battery,
        Stat::CaseBattery,
        Stat::SkinTemp,
        Stat::HeartRate,
        Stat::Hrv,
        Stat::Spo2,
        Stat::RespRate,
        Stat::DailySteps,
        Stat::StepsToGoal,
        Stat::DaysSinceSync,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Stat::Battery => "battery",
            Stat::CaseBattery => "case_battery",
            Stat::SkinTemp => "skin_temp",
            Stat::HeartRate => "heart_rate",
            Stat::Hrv => "hrv",
            Stat::Spo2 => "spo2",
            Stat::RespRate => "resp_rate",
            Stat::DailySteps => "daily_steps",
            Stat::StepsToGoal => "steps_to_goal",
            Stat::DaysSinceSync => "days_since_sync",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.key() == key)
    }

    pub fn label(self, fahrenheit: bool) -> &'static str {
        match self {
            Stat::Battery => "Battery (%)",
            Stat::CaseBattery => "Case battery (%)",
            Stat::SkinTemp if fahrenheit => "Skin temperature (°F)",
            Stat::SkinTemp => "Skin temperature (°C)",
            Stat::HeartRate => "Heart rate (bpm)",
            Stat::Hrv => "HRV (ms)",
            Stat::Spo2 => "Blood oxygen (%)",
            Stat::RespRate => "Respiratory rate (breaths/min)",
            Stat::DailySteps => "Daily steps",
            Stat::StepsToGoal => "Steps until goal",
            Stat::DaysSinceSync => "Days since last sync",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Stat::Battery => "Ring battery",
            Stat::CaseBattery => "Case battery",
            Stat::SkinTemp => "Skin temperature",
            Stat::HeartRate => "Heart rate",
            Stat::Hrv => "Heart rate variability",
            Stat::Spo2 => "Blood oxygen",
            Stat::RespRate => "Respiratory rate",
            Stat::DailySteps => "Steps today",
            Stat::StepsToGoal => "Steps until goal",
            Stat::DaysSinceSync => "No sync",
        }
    }

    /// Values from history arrive with each sync, so their alerts say when the reading was taken.
    pub fn is_history(self) -> bool {
        matches!(self, Stat::HeartRate | Stat::Hrv | Stat::Spo2 | Stat::RespRate)
    }

    pub fn format(self, value: f64, fahrenheit: bool) -> String {
        match self {
            Stat::Battery | Stat::CaseBattery | Stat::Spo2 => format!("{value:.0} %"),
            Stat::SkinTemp if fahrenheit => format!("{:.1} °F", to_fahrenheit(value)),
            Stat::SkinTemp => format!("{value:.1} °C"),
            Stat::HeartRate => format!("{value:.0} bpm"),
            Stat::Hrv => format!("{value:.0} ms"),
            Stat::RespRate => format!("{value:.1} breaths/min"),
            Stat::DailySteps | Stat::StepsToGoal => format!("{value:.0}"),
            Stat::DaysSinceSync if value.round() == 1.0 => "1 day".into(),
            Stat::DaysSinceSync => format!("{value:.0} days"),
        }
    }
}

pub fn to_fahrenheit(celsius: f64) -> f64 {
    celsius * 9.0 / 5.0 + 32.0
}

pub fn to_celsius(fahrenheit: f64) -> f64 {
    (fahrenheit - 32.0) * 5.0 / 9.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Lt,
    Le,
    Ge,
    Gt,
}

impl Op {
    pub const ALL: [Op; 4] = [Op::Lt, Op::Le, Op::Ge, Op::Gt];

    pub fn symbol(self) -> &'static str {
        match self {
            Op::Lt => "<",
            Op::Le => "≤",
            Op::Ge => "≥",
            Op::Gt => ">",
        }
    }

    pub fn from_symbol(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.symbol() == s)
    }

    pub fn test(self, value: f64, threshold: f64) -> bool {
        match self {
            Op::Lt => value < threshold,
            Op::Le => value <= threshold,
            Op::Ge => value >= threshold,
            Op::Gt => value > threshold,
        }
    }
}

/// Skin temperature thresholds are stored in °C whatever unit is shown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rule {
    pub id: i64,
    pub stat: Stat,
    pub op: Op,
    pub threshold: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub rule_id: i64,
    pub title: String,
    pub body: String,
}

/// Minimum time between two alerts from one rule.
pub const ALERT_GAP: i64 = 30 * 60;

#[derive(Debug, Clone, Copy)]
struct RuleState {
    armed: bool,
    last_fired: Option<i64>,
}

/// A rule fires when its condition becomes true, then not again until the condition has been
/// false in between, and never twice within `ALERT_GAP`.
#[derive(Debug, Default)]
pub struct Alerts {
    state: HashMap<i64, RuleState>,
}

impl Alerts {
    /// `taken` is when the reading was made, `now` the current time.
    pub fn check(&mut self, rules: &[Rule], stat: Stat, value: f64, taken: i64, now: i64, fahrenheit: bool) -> Vec<Alert> {
        let mut alerts = Vec::new();
        for rule in rules.iter().filter(|r| r.stat == stat) {
            let state = self.state.entry(rule.id).or_insert(RuleState { armed: true, last_fired: None });
            if !rule.op.test(value, rule.threshold) {
                state.armed = true;
                continue;
            }
            if !state.armed || state.last_fired.is_some_and(|t| now - t < ALERT_GAP) {
                continue;
            }
            state.armed = false;
            state.last_fired = Some(now);
            let reading = stat.format(value, fahrenheit);
            let title = if stat == Stat::StepsToGoal && value <= 0.0 {
                "Step goal reached".to_string()
            } else {
                format!("{} {reading}", stat.name())
            };
            let threshold = stat.format(rule.threshold, fahrenheit);
            let mut body = format!("{} {threshold}", rule.op.symbol());
            if stat.is_history() {
                if let Some(t) = chrono::TimeZone::timestamp_opt(&chrono::Local, taken, 0).single() {
                    body.push_str(&format!(" · measured {}", t.format("%-d %b %H:%M")));
                }
            }
            alerts.push(Alert { rule_id: rule.id, title, body });
        }
        alerts
    }

    pub fn forget(&mut self, rule_id: i64) {
        self.state.remove(&rule_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn battery_rule() -> Rule {
        Rule { id: 1, stat: Stat::Battery, op: Op::Lt, threshold: 5.0 }
    }

    #[test]
    fn settings_round_trip() {
        let s = Settings {
            sync_interval_minutes: None,
            fahrenheit: true,
            step_goal: 8000,
            sleep_goal_minutes: 450,
            case_always: true,
            profile: Profile { birth_year: Some(1981), reference: Reference::Female, height_cm: Some(177.8), weight_kg: Some(81.25) },
            us_body_units: true,
            ..Default::default()
        };
        let pairs = s.to_pairs().into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        assert_eq!(Settings::from_pairs(&pairs), s);
        assert_eq!(Settings::from_pairs(&HashMap::new()), Settings::default());
    }

    #[test]
    fn profile_entries() {
        assert_eq!(parse_birth_year("", 2026), Ok(None));
        assert_eq!(parse_birth_year(" 1981 ", 2026), Ok(Some(1981)));
        assert!(parse_birth_year("2027", 2026).is_err() && parse_birth_year("1899", 2026).is_err());

        let cm = |t: &str, us| parse_height(t, us).unwrap().map(|v| (v * 10.0).round() / 10.0);
        assert_eq!(cm("", true), None);
        assert_eq!(cm("178", false), Some(178.0));
        for text in ["70", "70 in", "70\""] {
            assert_eq!(cm(text, true), Some(177.8), "{text}");
        }
        assert!(parse_height("5 10", true).is_err() && parse_height("6", true).is_err() && parse_height("30", false).is_err());
        assert_eq!(height_text(177.8, true), "70");
        assert_eq!(height_text(177.8, false), "178");

        let kg = |t: &str, us| parse_weight(t, us).unwrap().map(|v| (v * 10.0).round() / 10.0);
        assert_eq!(kg("180 lb", true), Some(81.6));
        assert_eq!(kg("81,5", false), Some(81.5));
        assert!(parse_weight("5", false).is_err());
        assert_eq!(weight_text(81.65, true), "180");
        assert_eq!(weight_text(80.0, false), "80");
    }

    #[test]
    fn old_intervals_become_offered_ones() {
        let pairs = [("sync_interval_hours".to_string(), "6".to_string())].into_iter().collect();
        assert_eq!(Settings::from_pairs(&pairs).sync_interval_minutes, Some(90));
        for (saved, offered) in [("180", Some(90)), ("1440", Some(90)), ("15", Some(15)), ("45", Some(30)), ("never", None)] {
            let pairs = [("sync_interval_minutes".to_string(), saved.to_string())].into_iter().collect();
            assert_eq!(Settings::from_pairs(&pairs).sync_interval_minutes, offered, "{saved}");
        }
    }

    #[test]
    fn days_without_sync() {
        assert_eq!(Stat::DaysSinceSync.format(1.2, false), "1 day");
        assert_eq!(Stat::DaysSinceSync.format(5.0, false), "5 days");
    }

    #[test]
    fn fires_once_per_crossing() {
        let rules = [battery_rule()];
        let mut a = Alerts::default();
        assert_eq!(a.check(&rules, Stat::Battery, 4.0, 0, 0, false).len(), 1);
        assert!(a.check(&rules, Stat::Battery, 3.0, 0, 60, false).is_empty(), "still low: no repeat");
        assert!(a.check(&rules, Stat::Battery, 50.0, 0, 120, false).is_empty());
        assert!(a.check(&rules, Stat::Battery, 4.0, 0, 180, false).is_empty(), "crossed again within the gap");
        assert_eq!(a.check(&rules, Stat::Battery, 4.0, 0, ALERT_GAP + 1, false).len(), 1, "after the gap");
    }

    #[test]
    fn other_stats_are_ignored() {
        let rules = [battery_rule()];
        assert!(Alerts::default().check(&rules, Stat::CaseBattery, 1.0, 0, 0, false).is_empty());
    }

    #[test]
    fn history_alerts_carry_the_reading_time() {
        let rules = [Rule { id: 2, stat: Stat::HeartRate, op: Op::Gt, threshold: 120.0 }];
        let alert = &Alerts::default().check(&rules, Stat::HeartRate, 135.0, 1_792_600_000, 1_792_610_000, false)[0];
        assert_eq!(alert.title, "Heart rate 135 bpm");
        assert!(alert.body.starts_with("> 120 bpm · measured "), "{}", alert.body);
    }

    #[test]
    fn temperatures_show_in_the_chosen_unit() {
        assert_eq!(Stat::SkinTemp.format(37.0, true), "98.6 °F");
        assert!((to_celsius(to_fahrenheit(33.4)) - 33.4).abs() < 1e-9);
    }
}
