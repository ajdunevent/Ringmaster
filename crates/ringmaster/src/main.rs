mod autostart;
mod backend;
mod import;
mod settings;
mod sleep;
mod store;
mod ui;

use adw::prelude::*;
use std::path::PathBuf;

const APP_ID: &str = "net.dunevent.Ringmaster";

const USAGE: &str = "\
Usage: ringmaster [--hidden | --import-probe-report FILE | --import-ringconn-app FILE]

  --hidden                    start without a window, as at login
  --import-probe-report FILE  copy the history pages in a ringprobe report into the database
  --import-ringconn-app FILE  copy all history from the official app's ring_conn.db into the
                              database, for the most recently synced ring";

/// Distrobox leaves the host's system bus, where BlueZ lives, out of the container; it stays
/// reachable under `/run/host`. Has to run before any other thread starts.
fn use_host_system_bus() {
    const CONTAINER: &str = "/run/dbus/system_bus_socket";
    const HOST: &str = "/run/host/run/dbus/system_bus_socket";
    if std::env::var_os("DBUS_SYSTEM_BUS_ADDRESS").is_none()
        && !std::path::Path::new(CONTAINER).exists()
        && std::path::Path::new(HOST).exists()
    {
        std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", format!("unix:path={HOST}"));
    }
}

pub(crate) fn database_path() -> PathBuf {
    glib::user_data_dir().join("ringmaster").join("ringmaster.db")
}

fn main() -> glib::ExitCode {
    use_host_system_bus();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "ringmaster=info,ringconn=info".into()))
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let hidden = match args.as_slice() {
        [] => false,
        [flag] if flag == "--hidden" => true,
        [flag, file] if flag == "--import-probe-report" => return import(file),
        [flag, file] if flag == "--import-ringconn-app" => return import_official(file),
        _ => {
            eprintln!("{USAGE}");
            return glib::ExitCode::FAILURE;
        }
    };

    let app = adw::Application::builder().application_id(APP_ID).build();
    let window: std::rc::Rc<std::cell::RefCell<Option<adw::ApplicationWindow>>> = Default::default();
    // The guard has to be kept: dropping it releases the hold at once.
    let hold: std::rc::Rc<std::cell::RefCell<Option<gio::ApplicationHoldGuard>>> = Default::default();
    let first = std::cell::Cell::new(true);
    app.connect_activate(move |app| {
        if let Some(existing) = window.borrow().as_ref() {
            hold.take();
            existing.present();
            return;
        }
        let (commands, events) = backend::spawn(database_path());
        let store = match store::Store::open(&database_path()) {
            Ok(s) => Some(std::rc::Rc::new(s)),
            Err(e) => {
                tracing::error!(error = %e, "cannot open the database for reading");
                None
            }
        };
        let new_window = ui::build_window(app, commands, events, store);
        if hidden && first.get() {
            hold.replace(Some(app.hold()));
        } else {
            new_window.present();
        }
        first.set(false);
        window.replace(Some(new_window));
    });
    // GTK would otherwise parse leftover arguments as files to open.
    app.run_with_args(&[] as &[&str])
}

fn import_official(file: &str) -> glib::ExitCode {
    let path = database_path();
    let result = store::Store::open(&path).and_then(|mut s| {
        let ring = s.default_ring()?.ok_or_else(|| anyhow::anyhow!("connect a ring in Ringmaster first; its history goes to that ring"))?;
        import::official_app(&mut s, &ring, std::path::Path::new(file)).map(|summary| (ring, summary))
    });
    match result {
        Ok((ring, s)) => {
            println!(
                "Imported {} readings for {ring} ({} new), {} step counts and {} records of the app's own results",
                s.epochs, s.new_epochs, s.steps, s.records
            );
            glib::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("import failed: {e:#}");
            glib::ExitCode::FAILURE
        }
    }
}

fn import(file: &str) -> glib::ExitCode {
    let path = database_path();
    let result = store::Store::open(&path).and_then(|mut s| import::probe_report(&mut s, std::path::Path::new(file)));
    match result {
        Ok(s) => {
            println!(
                "Imported {} pages for {} into {} ({} new rows, {} damaged pages skipped)",
                s.pages,
                s.ring,
                path.display(),
                s.new_rows,
                s.skipped
            );
            glib::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("import failed: {e:#}");
            glib::ExitCode::FAILURE
        }
    }
}
