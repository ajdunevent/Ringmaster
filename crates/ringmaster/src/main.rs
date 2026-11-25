mod autostart;
mod backend;
mod import;
mod settings;
mod sleep;
mod stages;
mod status;
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

use std::cell::{Cell, RefCell};
use std::rc::Rc;

fn main() -> glib::ExitCode {
    use_host_system_bus();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "ringmaster=info,ringconn=info".into()))
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => {}
        [flag] if flag == "--hidden" => return headless(),
        [flag, file] if flag == "--import-probe-report" => return import(file),
        [flag, file] if flag == "--import-ringconn-app" => return import_official(file),
        _ => {
            eprintln!("{USAGE}");
            return glib::ExitCode::FAILURE;
        }
    };

    let app = adw::Application::builder().application_id(APP_ID).build();
    let window: Rc<RefCell<Option<ui::Window>>> = Default::default();
    let backend: Rc<RefCell<Option<async_channel::Sender<backend::Command>>>> = Default::default();
    let to_headless = Rc::new(Cell::new(false));
    app.connect_activate(move |app| {
        if let Some(existing) = window.borrow().as_ref() {
            to_headless.set(false);
            existing.window.present();
            return;
        }
        let (commands, events) = backend::spawn(database_path());
        backend.replace(Some(commands.clone()));
        {
            let (window, to_headless) = (window.clone(), to_headless.clone());
            glib::MainContext::default().spawn_local(async move {
                while let Ok(event) = events.recv().await {
                    if let Some(w) = window.borrow().as_ref() {
                        w.apply(event);
                    }
                }
                // The backend has stopped and its link is closed.
                if to_headless.get() {
                    replace_process(&["--hidden"]);
                }
            });
        }
        let store = match store::Store::open(&database_path()) {
            Ok(s) => Some(Rc::new(s)),
            Err(e) => {
                tracing::error!(error = %e, "cannot open the database for reading");
                None
            }
        };
        // Closing the window while Ringmaster keeps running replaces this process with one that
        // runs only the backend, once the backend here has closed its link: a process with GTK
        // and its renderer loaded holds far more memory than the backend needs.
        let to_background: Rc<dyn Fn()> = {
            let (to_headless, commands) = (to_headless.clone(), commands.clone());
            Rc::new(move || {
                to_headless.set(true);
                let _ = commands.try_send(backend::Command::Shutdown);
            })
        };
        let built = ui::build_window(app, commands, store, to_background);
        built.window.present();
        window.replace(Some(built));
    });
    // GTK would otherwise parse leftover arguments as files to open.
    app.run_with_args(&[] as &[&str])
}

/// Runs only the backend, with no window and without loading GTK, as at login or after the window
/// closes. Opening Ringmaster replaces this process with one that has a window, once the backend
/// here has closed its link; until then alerts still arrive as notifications. The desktop's list
/// of background apps shows what the ring is doing.
fn headless() -> glib::ExitCode {
    let app = gio::Application::builder().application_id(APP_ID).build();
    let (commands, events) = backend::spawn(database_path());
    let _hold = app.hold();
    let wanted = Rc::new(Cell::new(false));
    // `run` activates the application once at start; only later activations ask for a window.
    let started = Cell::new(false);
    {
        let wanted = wanted.clone();
        app.connect_activate(move |_| {
            if started.replace(true) {
                wanted.set(true);
                let _ = commands.try_send(backend::Command::Shutdown);
            }
        });
    }
    let status = Rc::new(RefCell::new(status::Status::default()));
    let portal = Rc::new(RefCell::new(status::Portal::new()));
    // Times in the status read "yesterday" once the day changes.
    {
        let (status, portal) = (status.clone(), portal.clone());
        glib::timeout_add_seconds_local(60, move || {
            portal.borrow_mut().show(status.borrow().text(chrono::Local::now()));
            glib::ControlFlow::Continue
        });
    }
    {
        let app = app.clone();
        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = events.recv().await {
                if status.borrow_mut().apply(&event) {
                    portal.borrow_mut().show(status.borrow().text(chrono::Local::now()));
                }
                if let backend::Event::Notify(alert) = event {
                    ui::notify(&app, &alert);
                }
            }
            if wanted.get() {
                replace_process(&[]);
            }
            app.quit();
        });
    }
    app.run_with_args(&[] as &[&str])
}

/// Replaces this process with Ringmaster started with `args`.
fn replace_process(args: &[&str]) {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ringmaster"));
    let error = std::process::Command::new(exe).args(args).exec();
    tracing::error!(error = %error, "cannot restart Ringmaster");
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
