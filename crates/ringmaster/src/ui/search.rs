//! The Add Ring dialog: open from the start of the search, it shows the search, then the rings
//! found or why none were.

use crate::backend::{Command, RingChoice};
use adw::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub(super) struct RingSearch {
    dialog: adw::Dialog,
    stack: gtk::Stack,
    body: gtk::Label,
    list: gtk::ListBox,
    none_title: gtk::Label,
    none_description: gtk::Label,
    picked: Rc<RefCell<Option<String>>>,
    /// A search was started and the dialog is still open for its answer.
    waiting: Cell<bool>,
    open: Cell<bool>,
    send: Rc<dyn Fn(Command)>,
}

fn page() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .valign(gtk::Align::Center)
        .margin_top(36)
        .margin_bottom(36)
        .margin_start(24)
        .margin_end(24)
        .build()
}

fn wrapped(class: &str) -> gtk::Label {
    gtk::Label::builder().css_classes([class]).wrap(true).justify(gtk::Justification::Center).build()
}

fn spinner_page(title: &str, caption: &str) -> gtk::Box {
    let page = page();
    let spinner = gtk::Spinner::builder().spinning(true).width_request(32).height_request(32).halign(gtk::Align::Center).build();
    page.append(&spinner);
    let title_label = wrapped("title-4");
    title_label.set_text(title);
    page.append(&title_label);
    if !caption.is_empty() {
        let caption_label = wrapped("dim-label");
        caption_label.set_text(caption);
        page.append(&caption_label);
    }
    page
}

/// A small dialog that cannot be dismissed, with a spinner and `title`, for work that ends on its
/// own.
pub(super) fn busy(title: &str) -> adw::Dialog {
    adw::Dialog::builder().can_close(false).content_width(300).child(&spinner_page(title, "")).build()
}

/// Capitalises the first letter of a backend message.
pub(super) fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

impl RingSearch {
    pub(super) fn new(send: Rc<dyn Fn(Command)>) -> Rc<Self> {
        let stack =
            gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vhomogeneous(false).interpolate_size(true).build();
        stack.add_named(&spinner_page("Searching for rings…", "Keep the ring close to this device."), Some("searching"));

        let body = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).css_classes(["dim-label"]).build();
        let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
        let pair = gtk::Button::builder().label("Pair").halign(gtk::Align::Center).css_classes(["pill", "suggested-action"]).build();
        let found = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        found.append(&body);
        found.append(&list);
        found.append(&pair);
        stack.add_named(&found, Some("found"));

        let again =
            gtk::Button::builder().label("Search again").halign(gtk::Align::Center).css_classes(["pill", "suggested-action"]).build();
        let (none_title, none_description) = (wrapped("title-4"), wrapped("dim-label"));
        let none = page();
        none.append(&none_title);
        none.append(&none_description);
        again.set_margin_top(12);
        none.append(&again);
        stack.add_named(&none, Some("none"));

        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&stack));
        let dialog = adw::Dialog::builder().title("Add a ring").content_width(360).child(&view).build();

        let search = Rc::new(Self {
            dialog,
            stack,
            body,
            list,
            none_title,
            none_description,
            picked: Default::default(),
            waiting: Cell::new(false),
            open: Cell::new(false),
            send,
        });
        {
            let s = search.clone();
            search.dialog.connect_closed(move |_| {
                s.waiting.set(false);
                s.open.set(false);
            });
        }
        {
            let s = search.clone();
            pair.connect_clicked(move |_| {
                if let Some(address) = s.picked.borrow().clone() {
                    (s.send)(Command::ConnectTo(address));
                }
                s.dialog.close();
            });
        }
        {
            let s = search.clone();
            again.connect_clicked(move |_| s.search());
        }
        search
    }

    /// Opens the dialog over `from` and starts a search.
    pub(super) fn start(&self, from: &impl IsA<gtk::Widget>) {
        self.search();
        self.present(from);
    }

    fn present(&self, from: &impl IsA<gtk::Widget>) {
        if !self.open.replace(true) {
            super::present_sheet(&self.dialog, from);
        }
    }

    fn search(&self) {
        self.waiting.set(true);
        self.stack.set_visible_child_name("searching");
        (self.send)(Command::AddRing);
    }

    /// The answer to a search; ignored once the dialog has been closed.
    pub(super) fn finished(&self, result: Result<Vec<RingChoice>, String>) {
        if !self.waiting.replace(false) {
            return;
        }
        match result {
            Ok(choices) if !choices.is_empty() => self.show_choices(&choices),
            Ok(_) => self.show_none(
                "No new rings found",
                "Seat the ring in its case or on its charger for a moment, take it off and search again. Rings already added are not listed.",
            ),
            Err(e) => self.show_none("Searching failed", &sentence(&e)),
        }
    }

    /// Rings a connection found nearby, with the dialog opened over `from` if it is not open.
    pub(super) fn choose(&self, choices: &[RingChoice], from: &impl IsA<gtk::Widget>) {
        self.waiting.set(false);
        self.show_choices(choices);
        self.present(from);
    }

    fn show_none(&self, title: &str, description: &str) {
        self.none_title.set_text(title);
        self.none_description.set_text(description);
        self.stack.set_visible_child_name("none");
    }

    fn show_choices(&self, choices: &[RingChoice]) {
        self.body.set_text(match choices.len() {
            1 => "Pair this ring with Ringmaster? It becomes the ring shown, synced and connected.",
            _ => "Choose yours. The strongest signal is usually the ring closest to this device.",
        });
        self.list.remove_all();
        self.picked.replace(None);
        let mut first: Option<gtk::CheckButton> = None;
        // A lone ring has nothing to choose between, so it gets the ring icon, not a check box.
        if let [c] = choices {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&c.name))
                .subtitle(format!("{} · {} dBm", c.address, c.rssi))
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("ringmaster-ring-symbolic"));
            self.list.append(&row);
            self.picked.replace(Some(c.address.clone()));
            self.stack.set_visible_child_name("found");
            return;
        }
        for (i, c) in choices.iter().enumerate() {
            let check = gtk::CheckButton::new();
            if let Some(f) = &first {
                check.set_group(Some(f));
            }
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&c.name))
                .subtitle(format!("{} · {} dBm{}", c.address, c.rssi, if i == 0 && choices.len() > 1 { " · strongest" } else { "" }))
                .activatable_widget(&check)
                .build();
            row.add_prefix(&check);
            let (picked, address) = (self.picked.clone(), c.address.clone());
            check.connect_toggled(move |b| {
                if b.is_active() {
                    picked.replace(Some(address.clone()));
                }
            });
            if i == 0 {
                check.set_active(true);
                first = Some(check);
            }
            self.list.append(&row);
        }
        self.stack.set_visible_child_name("found");
    }
}
