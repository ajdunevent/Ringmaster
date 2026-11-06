//! Pull to refresh for touch, adapted from Salutations: dragging down while the page is scrolled
//! to its top reveals an indicator above it; letting go past the threshold starts a refresh,
//! shown by a spinner until `finish`.
//!
//! The page's own touch scrolling takes the touch once it moves a few pixels, which would cancel
//! a pull, so a downward move from the top takes the touch first (the page cannot scroll up from
//! there anyway); any other first move leaves the touch to the page.

use adw::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

/// How far, in pixels, a pull must go to refresh.
const THRESHOLD: f64 = 96.0;
/// How far it goes before the indicator shows.
const SHOW_AT: f64 = 16.0;
/// How far a touch moves before it is decided to be a pull or not; below GTK's drag threshold,
/// at which the page starts scrolling.
const DECIDE_AT: f64 = 4.0;

pub struct PullRefresh {
    /// Goes above the page.
    pub indicator: gtk::Revealer,
    icons: gtk::Stack,
    arrow: gtk::Image,
    spinner: gtk::Widget,
    busy: Cell<bool>,
}

/// libadwaita's animated spinner where the system has it (1.6 and later), else GTK's. The build
/// targets 1.5, so the type is looked up by name at run time: GtkBuilder finds
/// `adw_spinner_get_type` in the loaded library.
fn spinner() -> gtk::Widget {
    let builder = gtk::Builder::new();
    let ui = r#"<interface><object class="AdwSpinner" id="spinner"/></interface>"#;
    if builder.add_from_string(ui).is_ok() {
        if let Some(w) = builder.object::<gtk::Widget>("spinner") {
            return w;
        }
    }
    gtk::Spinner::new().upcast()
}

impl PullRefresh {
    /// Watches touches on `scrolled`. `enabled` says whether a pull may refresh now, and
    /// `on_refresh` starts the refresh, which ends with `finish`.
    pub fn new(scrolled: &gtk::ScrolledWindow, enabled: impl Fn() -> bool + 'static, on_refresh: impl Fn() + 'static) -> Rc<Self> {
        let arrow = gtk::Image::from_icon_name("view-refresh-symbolic");
        let spinner = spinner();
        let icons = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
        icons.add_named(&arrow, Some("arrow"));
        icons.add_named(&spinner, Some("spinner"));
        icons.set_size_request(24, 24);
        icons.set_margin_top(12);
        icons.set_margin_bottom(12);
        let indicator = gtk::Revealer::builder().transition_type(gtk::RevealerTransitionType::SlideDown).child(&icons).build();
        let this = Rc::new(PullRefresh { indicator, icons, arrow, spinner, busy: Cell::new(false) });

        let drag = gtk::GestureDrag::builder().touch_only(true).build();
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);
        let armed = Rc::new(Cell::new(false));
        let (weak, a, page) = (Rc::downgrade(&this), armed.clone(), scrolled.clone());
        drag.connect_drag_begin(move |g, _, _| {
            let Some(this) = weak.upgrade() else { return };
            let ok = page.vadjustment().value() <= 0.5 && !this.busy.get() && enabled();
            a.set(ok);
            if !ok {
                g.set_state(gtk::EventSequenceState::Denied);
            }
        });
        let (weak, a) = (Rc::downgrade(&this), armed.clone());
        drag.connect_drag_update(move |g, dx, dy| {
            let Some(this) = weak.upgrade() else { return };
            if !a.get() {
                return;
            }
            let claimed = g.current_sequence().is_some_and(|q| g.sequence_state(&q) == gtk::EventSequenceState::Claimed);
            if !claimed {
                if dx.abs().max(dy.abs()) < DECIDE_AT {
                    return;
                }
                // Sideways or upward first: scrolling, not a pull.
                if dy <= 0.0 || dx.abs() > dy {
                    a.set(false);
                    g.set_state(gtk::EventSequenceState::Denied);
                    return;
                }
                g.set_state(gtk::EventSequenceState::Claimed);
            }
            // Back above the start: the pull is abandoned on release.
            if dy < 0.0 {
                this.indicator.set_reveal_child(false);
                this.arrow.remove_css_class("accent");
                return;
            }
            this.indicator.set_reveal_child(dy >= SHOW_AT);
            if dy >= THRESHOLD {
                this.arrow.add_css_class("accent");
            } else {
                this.arrow.remove_css_class("accent");
            }
        });
        // Taken over by another gesture: no refresh.
        let (weak, a) = (Rc::downgrade(&this), armed.clone());
        drag.connect_cancel(move |_, _| {
            if let Some(this) = weak.upgrade() {
                if a.replace(false) {
                    this.arrow.remove_css_class("accent");
                    this.indicator.set_reveal_child(false);
                }
            }
        });
        let weak = Rc::downgrade(&this);
        drag.connect_drag_end(move |_, _, dy| {
            let Some(this) = weak.upgrade() else { return };
            if !armed.replace(false) {
                return;
            }
            this.arrow.remove_css_class("accent");
            if dy < THRESHOLD {
                this.indicator.set_reveal_child(false);
                return;
            }
            this.busy.set(true);
            if let Some(s) = this.spinner.downcast_ref::<gtk::Spinner>() {
                s.start();
            }
            this.icons.set_visible_child_name("spinner");
            this.indicator.set_reveal_child(true);
            on_refresh();
        });
        scrolled.add_controller(drag);
        this
    }

    /// Ends the refresh: hides the spinner. Does nothing when no pull is under way.
    pub fn finish(&self) {
        if !self.busy.replace(false) {
            return;
        }
        self.indicator.set_reveal_child(false);
        let (icons, spinner) = (self.icons.clone(), self.spinner.clone());
        // Back to the arrow once hidden, so the spinner does not turn into an arrow in view.
        glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
            if let Some(s) = spinner.downcast_ref::<gtk::Spinner>() {
                s.stop();
            }
            icons.set_visible_child_name("arrow");
        });
    }
}
