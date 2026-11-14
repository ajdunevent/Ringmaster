//! The list of rings shared by the main page's ring picker and Preferences.

use crate::store::{RingEntry, COLMI, ULTRAHUMAN};

pub(super) fn brand_name(entry: &RingEntry) -> &'static str {
    match entry.info.brand.as_deref() {
        Some(COLMI) => "Colmi",
        Some(ULTRAHUMAN) => "Ultrahuman",
        _ => "RingConn",
    }
}

/// "RingConn · synced Today 14:32", or "Ultrahuman · never synced".
pub(super) fn subtitle(entry: &RingEntry) -> String {
    let state = entry.last_sync.map_or_else(|| "never synced".into(), |t| format!("synced {}", super::when(t)));
    format!("{} · {state}", brand_name(entry))
}

/// Radio rows for `entries`, with `current` checked; `on_pick` gets the address chosen. A lone
/// ring has nothing to choose between, so it gets the ring icon instead of a lone check box.
pub(super) fn radio_rows(entries: &[RingEntry], current: Option<&str>, on_pick: impl Fn(String) + Clone + 'static) -> Vec<adw::ActionRow> {
    use adw::prelude::*;
    let mut first: Option<gtk::CheckButton> = None;
    let mut rows = Vec::new();
    if let [e] = entries {
        let row = adw::ActionRow::builder().title(glib::markup_escape_text(&e.info.display_name())).subtitle(subtitle(e)).build();
        row.add_prefix(&gtk::Image::from_icon_name("ringmaster-ring-symbolic"));
        return vec![row];
    }
    for e in entries {
        let check = gtk::CheckButton::new();
        if let Some(f) = &first {
            check.set_group(Some(f));
        } else {
            first = Some(check.clone());
        }
        check.set_active(current == Some(e.info.address.as_str()));
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&e.info.display_name()))
            .subtitle(subtitle(e))
            .activatable_widget(&check)
            .build();
        row.add_prefix(&check);
        let (on_pick, address) = (on_pick.clone(), e.info.address.clone());
        check.connect_toggled(move |b| {
            if b.is_active() {
                on_pick(address.clone());
            }
        });
        rows.push(row);
    }
    rows
}
