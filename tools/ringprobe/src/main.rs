mod explore;
mod overnight;
mod protocol;
mod report;
mod rwfit_probe;
mod transport;

use anyhow::{anyhow, bail, Context, Result};
use bluer::gatt::remote::Characteristic;
use bluer::{Address, AddressType, Device};
use chrono::{Local, TimeZone};
use protocol::{cmd, Descriptor, EpochLayout, CHANNEL_ALL_DAY, CHANNEL_SPORT};
use report::Report;
use ringconn::drain::{Action, Batch, ChannelDrain};
use ringconn::Channel;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use transport::Link;

/// Run 2 of 2026-10-21 saw the ring at -98 dBm and never connected; -62 dBm connected.
const WEAK_SIGNAL: i16 = -90;

/// Error text when the ring takes a connection but answers no command.
pub(crate) const NO_ANSWER: &str = "ring did not answer the status request";

const USAGE: &str = "\
ringprobe: scan, pair and authenticate with a RingConn ring, probe it, and write a report.

Usage: ringprobe [options]

  --address XX:XX:XX:XX:XX:XX  ring to use (default: the bonded ring, or the first one found)
  --listen SECONDS             passive listen for status frames (default 60)
  --skip-live                  skip the live heart-rate test
  --live-test                  only compare ways of starting a live measurement (about 4 min)
  --drain                      download history and acknowledge every page (see below)
  --overnight HOURS            back-to-back live SpO2 sessions for HOURS, then a short PPG test
                               (work mode 2, then 06 04 00 and 06 05 00); acknowledges no page,
                               so the ring keeps all its history; sync in Ringmaster afterwards
  --explore MINUTES            exploration run, MINUTES (3 to 30) plus up to 10 min: untried live
                               modes, factory sensor tests, apnea data requests, unseen history
                               channels, then MINUTES worn in apnea work mode and another try at
                               its data. Acknowledges only unknown page types; sync in Ringmaster
                               before and after
  --osa MINUTES                apnea monitoring run, MINUTES (3 to 60) plus about 6 min: switches
                               the ring's apnea monitoring on (parameter 05 22), wears it, tries
                               every data request, then switches it off. Acknowledges only
                               unknown page types; sync in Ringmaster before and after
  --ppg SECONDS                raw PPG the way the official app collects it for blood pressure:
                               work mode 2 (factory mode) and PPG 22 08 00 (25 Hz), restarted
                               whenever it stops, for SECONDS (10 to 600), with decoded samples as
                               CSV; then the accelerometer briefly and 20 01 00 to leave factory
                               mode. About SECONDS plus 1 min; nothing acknowledged
  --rwfit                      an RWfit ring (RWFit app family) instead of a RingConn: framing,
                               identity, binding, features, battery and every history stream;
                               reading history may remove it from the ring
  --raw-ppg                    with --overnight: raw PPG all night instead of SpO2 sessions
                               (factory-mode PPG sessions, each ended with 20 01 00 and
                               restarted, reconnecting as needed; stops if the ring stops
                               answering); samples go to <report>.ppg.csv, about 4 MB an hour
  --ppg-rest SECONDS           with --raw-ppg, wait after each session (default 5)
  --min-battery PERCENT        with --overnight, stop below this battery level (default 40)
  --ppg-seconds SECONDS        with --overnight, length of each PPG mode test (default 30; 0 skips)
  --pauses SECONDS,...         with --overnight, wait after each session, one value per 30-minute
                               block in turn (default 120,270,600); live sessions stop the ring's
                               own 2.5-minute records, and the pauses show how much they leave
  --risky                      required by --drain, --overnight, --explore, --osa, --ppg and
                               --rwfit, which can remove history from the ring, stop its own
                               recording or leave it unresponsive (see README.md)
  --out FILE                   report path (default ringprobe-<date>-<time>.txt); an existing
                               file is never overwritten

By default history is only peeked at: pages are read but never acknowledged, so the ring keeps
them. With --drain every page is written to the report before it is acknowledged, and the ring
then discards it; another app syncing this ring will not receive that data.

Wear the ring for the live heart-rate test. The report contains the ring's address and health
readings. Quit Ringmaster first (menu, Quit) so the two do not talk to the ring at once.

For --overnight, keep the phone on its charger near the bed and stop it from suspending, e.g.
  systemd-inhibit --what=sleep:idle ringprobe --overnight 3 --risky";

struct Options {
    address: Option<Address>,
    listen: u64,
    live: bool,
    live_test: bool,
    drain: bool,
    overnight: Option<overnight::Plan>,
    explore: Option<explore::Plan>,
    osa: Option<explore::OsaPlan>,
    ppg: Option<explore::PpgPlan>,
    rwfit: bool,
    out: PathBuf,
}

fn parse_args() -> Result<Options> {
    let mut o = Options {
        address: None,
        listen: 60,
        live: true,
        live_test: false,
        drain: false,
        overnight: None,
        explore: None,
        osa: None,
        ppg: None,
        rwfit: false,
        out: PathBuf::from(format!("ringprobe-{}.txt", Local::now().format("%Y%m%d-%H%M%S"))),
    };
    let (mut hours, mut min_battery, mut ppg_seconds) = (None, 40u8, 30u64);
    let mut raw_ppg = false;
    let mut ppg_rest = 5u64;
    let mut risky = false;
    let mut pauses: Vec<u64> = vec![120, 270, 600];
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--address" => o.address = Some(args.next().context("--address needs a value")?.parse().context("bad address")?),
            "--listen" => o.listen = args.next().context("--listen needs a value")?.parse().context("bad --listen")?,
            "--skip-live" => o.live = false,
            "--live-test" => o.live_test = true,
            "--drain" => o.drain = true,
            "--overnight" => {
                let h: f64 = args.next().context("--overnight needs a number of hours")?.parse().context("bad --overnight")?;
                if !(0.05..=12.0).contains(&h) {
                    bail!("--overnight takes 0.05 to 12 hours");
                }
                hours = Some(h);
            }
            "--explore" => {
                let m: u64 = args.next().context("--explore needs a number of minutes")?.parse().context("bad --explore")?;
                if !(3..=30).contains(&m) {
                    bail!("--explore takes 3 to 30 minutes");
                }
                o.explore = Some(explore::Plan { wear_minutes: m });
            }
            "--osa" => {
                let m: u64 = args.next().context("--osa needs a number of minutes")?.parse().context("bad --osa")?;
                if !(3..=60).contains(&m) {
                    bail!("--osa takes 3 to 60 minutes");
                }
                o.osa = Some(explore::OsaPlan { minutes: m });
            }
            "--ppg" => {
                let s: u64 = args.next().context("--ppg needs a number of seconds")?.parse().context("bad --ppg")?;
                if !(10..=600).contains(&s) {
                    bail!("--ppg takes 10 to 600 seconds");
                }
                o.ppg = Some(explore::PpgPlan { seconds: s });
            }
            "--raw-ppg" => raw_ppg = true,
            "--rwfit" => o.rwfit = true,
            "--risky" => risky = true,
            "--ppg-rest" => {
                ppg_rest = args.next().context("--ppg-rest needs a value")?.parse().context("bad --ppg-rest")?;
                if ppg_rest > 600 {
                    bail!("--ppg-rest takes 0 to 600 seconds");
                }
            }
            "--min-battery" => min_battery = args.next().context("--min-battery needs a value")?.parse().context("bad --min-battery")?,
            "--ppg-seconds" => ppg_seconds = args.next().context("--ppg-seconds needs a value")?.parse().context("bad --ppg-seconds")?,
            "--pauses" => {
                pauses = args
                    .next()
                    .context("--pauses needs a list of seconds")?
                    .split(',')
                    .map(|v| v.trim().parse::<u64>())
                    .collect::<Result<_, _>>()
                    .context("bad --pauses")?;
                if pauses.is_empty() || pauses.iter().any(|p| *p > 3600) {
                    bail!("--pauses takes 0 to 3600 seconds each");
                }
            }
            "--out" => o.out = PathBuf::from(args.next().context("--out needs a value")?),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other}\n\n{USAGE}"),
        }
    }
    if raw_ppg && hours.is_none() {
        bail!("--raw-ppg needs --overnight HOURS");
    }
    if [o.explore.is_some(), o.osa.is_some(), o.ppg.is_some(), o.drain, hours.is_some()].iter().filter(|b| **b).count() > 1 {
        bail!("--explore, --osa, --ppg, --drain and --overnight cannot be combined");
    }
    if o.drain && hours.is_some() {
        bail!("--drain and --overnight cannot be combined");
    }
    let wanted: Vec<&str> = [
        (o.drain, "--drain"),
        (hours.is_some(), "--overnight"),
        (o.explore.is_some(), "--explore"),
        (o.osa.is_some(), "--osa"),
        (o.ppg.is_some(), "--ppg"),
        (o.rwfit, "--rwfit"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect();
    if !wanted.is_empty() && !risky {
        bail!(
            "{} can remove history from the ring, stop its own recording or leave it unresponsive \
             until it is seated in its case; see tools/ringprobe/README.md, then add --risky",
            wanted.join(", ")
        );
    }
    o.overnight = hours.map(|hours| overnight::Plan { hours, min_battery, ppg_seconds, pauses: pauses.clone(), raw_ppg, ppg_rest });
    Ok(o)
}

#[derive(Default)]
struct Results {
    environment: Vec<String>,
    gatt: Option<String>,
    identity: Option<String>,
    pairing: Option<String>,
    auth: Option<String>,
    wake_fetch: Option<bool>,
    wake_query: Option<bool>,
    live: Option<String>,
    live_test: Vec<String>,
    channels: Vec<(u8, String)>,
    sync_unix: Option<i64>,
}

#[derive(Default)]
struct Shared {
    results: Mutex<Results>,
    device: Mutex<Option<Device>>,
    command: Mutex<Option<Characteristic>>,
    live_active: AtomicBool,
}

impl Shared {
    fn with<T>(&self, f: impl FnOnce(&mut Results) -> T) -> T {
        f(&mut self.results.lock().unwrap())
    }
}

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

fn main() -> std::process::ExitCode {
    use_host_system_bus();
    match tokio::runtime::Runtime::new() {
        Ok(rt) => rt.block_on(probe()),
        Err(e) => {
            eprintln!("cannot start the async runtime: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn probe() -> std::process::ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e:#}");
            return std::process::ExitCode::from(2);
        }
    };
    if overnight::ringmaster_running() {
        eprintln!("Ringmaster is running. Quit it first (menu, Quit) so the two do not talk to the ring at once.");
        return std::process::ExitCode::from(2);
    }
    let report = match Report::create(opts.out.clone()) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("cannot create {}: {e}", opts.out.display());
            return std::process::ExitCode::from(2);
        }
    };
    if opts.drain {
        report.note("--drain: history pages will be acknowledged and removed from the ring after being saved here");
    }

    let shared = Arc::new(Shared::default());
    let outcome = tokio::select! {
        r = async {
            if opts.rwfit {
                rwfit_probe::run(opts.address, report.clone(), shared.clone()).await
            } else {
                run(&opts, report.clone(), shared.clone()).await
            }
        } => r,
        _ = tokio::signal::ctrl_c() => Err(anyhow!("interrupted with Ctrl-C")),
    };
    let ok = outcome.is_ok();
    if let Err(e) = &outcome {
        report.note(format!("stopped: {e:#}"));
    }
    cleanup(&shared, &report).await;
    findings(&opts, &report, &shared, outcome.err().map(|e| format!("{e:#}")));

    let header = vec![
        format!("# ringprobe report, {}", Local::now().format("%Y-%m-%d %H:%M:%S %Z")),
        String::new(),
        format!(
            "ringprobe {} | history mode: {}",
            env!("CARGO_PKG_VERSION"),
            if opts.rwfit {
                "RWfit ring (history read and acknowledged frame by frame)"
            } else if opts.explore.is_some() || opts.osa.is_some() || opts.ppg.is_some() {
                "explore (only unknown page types acknowledged)"
            } else if opts.overnight.is_some() {
                "none (overnight test; no sync channel opened)"
            } else if opts.drain {
                "drain"
            } else {
                "peek (nothing acknowledged)"
            }
        ),
        "Contains the ring's Bluetooth address and health readings.".into(),
        "Section numbers refer to PROTOCOL.md in the Ringmaster project.".into(),
    ];
    match report.finish(&header) {
        Ok(()) => println!("\nReport written to {}", report.path().display()),
        Err(e) => eprintln!("\nwriting the report failed: {e}; the partial log is at {}", report.path().display()),
    }
    if ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

async fn cleanup(shared: &Shared, report: &Report) {
    let command = shared.command.lock().unwrap().clone();
    if shared.live_active.load(Ordering::SeqCst) {
        if let Some(c) = command {
            report.tx(&cmd::LIVE_OFF, "leave live mode (cleanup)");
            let _ = tokio::time::timeout(Duration::from_secs(3), c.write_ext(&cmd::LIVE_OFF, &transport::write_request())).await;
        }
    }
    let device = shared.device.lock().unwrap().clone();
    if let Some(d) = device {
        report.note("disconnecting (bond is kept)");
        let _ = tokio::time::timeout(Duration::from_secs(5), d.disconnect()).await;
    }
}

fn command_output(program: &str, args: &[&str]) -> String {
    std::process::Command::new(program)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unavailable".into())
}

fn os_name() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string())))
        .unwrap_or_else(|| "unknown".into())
}

async fn run(opts: &Options, report: Arc<Report>, shared: Arc<Shared>) -> Result<()> {
    shared.with(|r| {
        r.environment = vec![
            format!("OS             {}", os_name()),
            format!("kernel         {}", command_output("uname", &["-r"])),
            format!("bluetoothctl   {}", command_output("bluetoothctl", &["--version"])),
        ]
    });

    let session = bluer::Session::new().await.context("cannot reach BlueZ over D-Bus")?;
    let adapter = session.default_adapter().await.context("no Bluetooth adapter")?;
    adapter.set_powered(true).await.context("cannot power the adapter")?;
    let adapter_addr = adapter.address().await.map(|a| a.to_string()).unwrap_or_default();
    shared.with(|r| r.environment.push(format!("adapter        {} ({adapter_addr})", adapter.name())));
    let _agent = session.register_agent(transport::agent(report.clone())).await.context("cannot register pairing agent")?;
    report.note(format!("adapter {} ready, agent registered", adapter.name()));

    let mut known_ring = None;
    for addr in transport::known_addresses(&adapter).await? {
        let Ok(d) = adapter.device(addr) else { continue };
        let wanted = match opts.address {
            Some(a) => a == addr,
            None => transport::is_ring(&d).await,
        };
        if wanted {
            let paired = d.is_paired().await.unwrap_or(false);
            report.note(format!("BlueZ already knows {addr} (paired: {paired})"));
            if known_ring.is_none() || paired {
                known_ring = Some(addr);
            }
        }
    }

    report.note("scanning for up to 20 s; if nothing is found, seat the ring in its case for a moment and take it out");
    let sightings =
        transport::scan(&adapter, &report, Duration::from_secs(20), Duration::from_secs(3), opts.address.or(known_ring)).await?;
    let mut scan_lines = Vec::new();
    for s in &sightings {
        scan_lines.extend(s.lines.iter().cloned());
        scan_lines.push(String::new());
    }
    if sightings.is_empty() {
        scan_lines.push("no ring seen while scanning".into());
    }
    report.section("Advertisement", scan_lines);

    let target = match (opts.address, known_ring) {
        (Some(a), _) => a,
        (None, Some(a)) => a,
        (None, None) => match sightings.as_slice() {
            [] => bail!("no RingConn ring found; seat it in its case for a moment, take it out and run again"),
            [one] => one.address,
            many => {
                let list: Vec<_> = many.iter().map(|s| s.address.to_string()).collect();
                bail!("several rings found ({}); run again with --address", list.join(", "))
            }
        },
    };
    let device = adapter.device(target)?;
    *shared.device.lock().unwrap() = Some(device.clone());
    report.note(format!("using {target}"));
    match sightings.iter().find(|s| s.address == target).and_then(|s| s.rssi) {
        None if !device.is_connected().await.unwrap_or(false) => {
            report.note("no advertisement from the ring; it may be asleep or in its closed case. Trying to connect anyway")
        }
        Some(r) if r < WEAK_SIGNAL => report.note(format!("weak signal ({r} dBm); connecting may fail, bring the ring closer")),
        _ => {}
    }

    let address_type = device.address_type().await.ok();
    let paired_before = device.is_paired().await.unwrap_or(false);
    if let Err(e) = device.set_trusted(true).await {
        report.note(format!("set trusted failed: {e}"));
    }

    transport::connect(&device, &report).await?;

    let pairing = if paired_before {
        "already bonded before this run; pairing not exercised (remove the ring in Bluetooth settings and rerun to test it)".to_string()
    } else {
        report.note("pairing (Just Works expected)");
        let outcome = transport::pair(&device).await;
        let paired = device.is_paired().await.unwrap_or(false);
        report.note(format!("{outcome}; paired now: {paired}"));
        format!("{}: {outcome}", if paired { "BONDED" } else { "NOT BONDED (take the ring out of a closed case)" })
    };
    shared.with(|r| r.pairing = Some(pairing));

    if !transport::wait_ready(&device, &report, Duration::from_secs(30)).await {
        report.note("services not readable after 30 s; trying anyway");
    }
    let (table, values) = transport::gatt_table(&device).await.context("reading GATT layout failed")?;
    report.section("GATT layout", table.clone());
    gatt_check(&table, &shared);

    let lookup = |uuid: &str| values.iter().find(|(u, _)| u == uuid).map(|(_, v)| v.clone());
    let text = |uuid: &str| lookup(uuid).map(|v| String::from_utf8_lossy(&v).trim_end_matches('\0').to_string());
    let mut identity = vec![
        format!("manufacturer   {}", text("0x2a29").unwrap_or_else(|| "?".into())),
        format!("model          {}", text("0x2a24").unwrap_or_else(|| "?".into())),
        format!("serial         {}", text("0x2a25").unwrap_or_else(|| "?".into())),
        format!("firmware       {}", text("0x2a26").unwrap_or_else(|| "?".into())),
        format!("hardware       {}", text("0x2a27").unwrap_or_else(|| "?".into())),
        format!("software       {}", text("0x2a28").unwrap_or_else(|| "?".into())),
        format!("system id      {}", lookup("0x2a23").map(|v| protocol::hex(&v)).unwrap_or_else(|| "?".into())),
        format!("address        {target} ({})", address_type.map(|t| t.to_string()).unwrap_or_else(|| "type unknown".into())),
    ];
    if address_type == Some(AddressType::LeRandom) {
        identity.push("WARNING: random address; the authentication key comes from the MAC and may not match".into());
    }
    report.section("Device information", identity.clone());
    shared.with(|r| r.identity = Some(identity.join("\n")));

    let mut link = Link::open(&device, report.clone()).await?;
    *shared.command.lock().unwrap() = Some(link.command_char());

    authenticate(&mut link, &report, &shared, target).await?;
    wake_telemetry(&mut link, &shared).await?;

    if let Some(plan) = &opts.ppg {
        explore::run_ppg(&mut link, &report, plan).await?;
        report.note("probe complete");
        return Ok(());
    }
    if let Some(plan) = &opts.osa {
        explore::run_osa(&mut link, &report, plan).await?;
        report.note("probe complete");
        return Ok(());
    }
    if let Some(plan) = &opts.explore {
        explore::run(&mut link, &report, &shared, plan).await?;
        report.note("probe complete");
        return Ok(());
    }
    if let Some(plan) = &opts.overnight {
        overnight::run(&mut link, &device, target, &report, &shared, plan).await?;
        report.note("probe complete");
        return Ok(());
    }
    if opts.live_test {
        live_strategies(&mut link, &report, &shared).await?;
        report.note("probe complete");
        return Ok(());
    }

    report.note(format!("listening passively for {} s", opts.listen));
    link.collect(Duration::from_secs(opts.listen), Duration::from_secs(opts.listen)).await;

    if opts.live {
        live_heart_rate(&mut link, &report, &shared).await?;
    }

    for channel in [protocol::CHANNEL_SLEEP, protocol::CHANNEL_SPORT, protocol::CHANNEL_ALL_DAY] {
        history(&mut link, &report, &shared, channel, opts.drain).await?;
    }
    report.note("probe complete");
    Ok(())
}

fn gatt_check(table: &[String], shared: &Shared) {
    let find = |needle: &str| table.iter().find(|l| l.contains(needle)).cloned();
    let mut problems = Vec::new();
    if find(&protocol::DATA_SERVICE.to_string()).is_none() {
        problems.push("data service 8327ad99 missing".to_string());
    }
    match find(&protocol::COMMAND_CHAR.to_string()) {
        Some(l) if l.contains("write") => {}
        Some(l) => problems.push(format!("command char lacks write: {}", l.trim())),
        None => problems.push("command char 8327ad98 missing".into()),
    }
    match find(&protocol::NOTIFY_CHAR.to_string()) {
        Some(l) if l.contains("notify") => {}
        Some(l) => problems.push(format!("notify char lacks notify: {}", l.trim())),
        None => problems.push("notify char 8327ad97 missing".into()),
    }
    if find("0000180a-").is_none() {
        problems.push("Device Information service missing".into());
    }
    let secondary = if find(&protocol::SECONDARY_SERVICE.to_string()).is_some() { "present" } else { "absent" };
    let answer = if problems.is_empty() {
        format!("MATCHES section 2 (data service, command/notify pair, DIS). Secondary service 1d14d6ee {secondary}.")
    } else {
        format!("DIFFERS: {}. Secondary service 1d14d6ee {secondary}.", problems.join("; "))
    };
    shared.with(|r| r.gatt = Some(answer));
}

async fn authenticate(link: &mut Link, report: &Report, shared: &Shared, address: Address) -> Result<()> {
    link.send(&cmd::STATUS).await?;
    let Some(status) = link.wait_for(&[0x81], Duration::from_secs(5)).await else {
        shared.with(|r| r.auth = Some("FAILED: no 81 reply to 01 00 00".into()));
        bail!(NO_ANSWER);
    };
    if status.len() < 3 || status[1] != 0x00 {
        shared.with(|r| r.auth = Some(format!("UNEXPECTED: status reply {}", protocol::hex(&status))));
        bail!("unexpected status reply");
    }
    let challenge = status[2];
    let response = protocol::auth_response(address.0, challenge);
    report.note(format!(
        "challenge {challenge:02x}, V = {:02x}, response {}",
        address.0[3] ^ address.0[4] ^ address.0[5],
        protocol::hex(&response)
    ));
    link.send(&cmd::auth_reply(response)).await?;
    let answer = match link.wait_for(&[0x81], Duration::from_secs(5)).await {
        Some(f) if f.get(1) == Some(&0x01) => format!("81 01 reply received ({} bytes) to challenge {challenge:02x}", f.len()),
        Some(f) => format!("unexpected reply {}", protocol::hex(&f)),
        None => "no 81 01 reply".into(),
    };
    report.note(&answer);
    shared.with(|r| r.auth = Some(answer));
    Ok(())
}

async fn wake_telemetry(link: &mut Link, shared: &Shared) -> Result<()> {
    link.send(&cmd::FETCH).await?;
    let fetch = link.wait_for(&[0x87], Duration::from_secs(4)).await.is_some();
    tokio::time::sleep(Duration::from_secs(1)).await;
    link.send(&cmd::STATUS_QUERY).await?;
    let query = link.wait_for(&[0x10], Duration::from_secs(6)).await.is_some();
    shared.with(|r| {
        r.wake_fetch = Some(fetch);
        r.wake_query = Some(query);
    });
    Ok(())
}

async fn live_heart_rate(link: &mut Link, report: &Report, shared: &Shared) -> Result<()> {
    report.note("live heart rate: the ring must be worn");
    link.send(&cmd::STATUS_QUERY).await?;
    link.wait_for(&[0x10], Duration::from_secs(3)).await;
    link.send(&cmd::LIVE_HR).await?;
    shared.live_active.store(true, Ordering::SeqCst);
    let mode = link.wait_for(&[0x86], Duration::from_secs(3)).await;
    link.send(&cmd::FETCH).await?;

    // The ring pushes samples unprompted about every 2 s; listening alone and polling are
    // measured separately to see whether polls reset its averaging.
    report.note("live heart rate: 30 s listening only");
    let heard: Vec<u8> = link
        .collect(Duration::from_secs(30), Duration::from_secs(30))
        .await
        .iter()
        .filter(|f| f.first() == Some(&0x15) && f.get(1) == Some(&0x00) && f.len() > 2)
        .map(|f| f[2])
        .collect();

    report.note("live heart rate: 20 s polling every 2 s");
    let mut polled = Vec::new();
    for _ in 0..10 {
        let started = tokio::time::Instant::now();
        link.send(&cmd::POLL).await?;
        if let Some(f) = link.wait_for(&[0x15], Duration::from_secs(2)).await {
            if f.get(1) == Some(&0x00) && f.len() > 2 {
                polled.push(f[2]);
            }
        }
        // Polling faster than every 2 s resets the ring's averaging window.
        tokio::time::sleep_until(started + Duration::from_secs(2)).await;
    }
    link.send(&cmd::LIVE_OFF).await?;
    let off = link.wait_for(&[0x86], Duration::from_secs(3)).await;
    shared.live_active.store(false, Ordering::SeqCst);

    let describe = |s: &[u8]| {
        let locked: Vec<_> = s.iter().filter(|&&v| v >= 30).collect();
        format!(
            "{} samples {:?}, {} at or above 30 bpm{}",
            s.len(),
            s,
            locked.len(),
            locked.last().map(|v| format!(", last {v} bpm")).unwrap_or_default()
        )
    };
    let summary = format!(
        "mode reply: {}\nlistening only: {}\npolling: {}\nleave-mode reply: {}",
        mode.map(|f| protocol::hex(&f)).unwrap_or_else(|| "none".into()),
        describe(&heard),
        describe(&polled),
        off.map(|f| protocol::hex(&f)).unwrap_or_else(|| "none".into()),
    );
    report.note(format!("live heart rate: {summary}"));
    shared.with(|r| r.live = Some(summary));
    Ok(())
}

struct Strategy {
    name: &'static str,
    mode: [u8; 3],
    settle: bool,
    fetch: bool,
    /// Seconds after start of the first poll; `None` for no polling.
    poll_after: Option<u64>,
}

async fn live_strategies(link: &mut Link, report: &Report, shared: &Shared) -> Result<()> {
    report.note("live test: keep the ring snug and your hand still");
    let strategies = [
        Strategy {
            name: "A (ringlink): status query, 0.4 s, mode, 0.4 s, poll every 2 s",
            mode: cmd::LIVE_HR,
            settle: true,
            fetch: false,
            poll_after: Some(0),
        },
        Strategy {
            name: "B (OpenCircuit): status query, mode, fetch, poll every 2 s from 10 s",
            mode: cmd::LIVE_HR,
            settle: false,
            fetch: true,
            poll_after: Some(10),
        },
        Strategy { name: "C: status query, mode, fetch, no polls", mode: cmd::LIVE_HR, settle: false, fetch: true, poll_after: None },
        Strategy { name: "D blood oxygen, started as A", mode: cmd::LIVE_SPO2, settle: true, fetch: false, poll_after: Some(0) },
    ];
    for s in strategies {
        report.note(format!("live test {}", s.name));
        link.send(&cmd::STATUS_QUERY).await?;
        if s.settle {
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        link.send(&s.mode).await?;
        shared.live_active.store(true, Ordering::SeqCst);
        if s.settle {
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        if s.fetch {
            link.send(&cmd::FETCH).await?;
        }
        let start = tokio::time::Instant::now();
        let end = start + Duration::from_secs(40);
        let mut next_poll = s.poll_after.map(|a| start + Duration::from_secs(a));
        let mut values = Vec::new();
        loop {
            let now = tokio::time::Instant::now();
            if now >= end {
                break;
            }
            if let Some(p) = next_poll.filter(|p| now >= *p) {
                link.send(&cmd::POLL).await?;
                next_poll = Some(p + Duration::from_secs(2));
            }
            let until = next_poll.map_or(end, |p| p.min(end));
            let Some(f) = link.recv(until.saturating_duration_since(tokio::time::Instant::now())).await else { continue };
            if f.first() == Some(&0x15) && f.len() > 2 {
                let v = if f[1] == 0x01 { f.get(14).copied().unwrap_or(0) } else { f[2] };
                values.push(format!("{:.0}s:{v}", start.elapsed().as_secs_f64()));
            }
        }
        link.send(&cmd::LIVE_OFF).await?;
        link.wait_for(&[0x86], Duration::from_secs(3)).await;
        shared.live_active.store(false, Ordering::SeqCst);
        let line = format!("{}\n  {} samples: {}", s.name, values.len(), if values.is_empty() { "none".into() } else { values.join(" ") });
        report.note(&line);
        shared.with(|r| r.live_test.push(line));
        link.collect(Duration::from_secs(5), Duration::from_secs(5)).await;
    }
    Ok(())
}

async fn history(link: &mut Link, report: &Report, shared: &Shared, channel: u8, drain: bool) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    shared.with(|r| *r.sync_unix.get_or_insert(now));
    report.note(format!("channel {channel:02x}: sync open at cursor {:08x}", protocol::cursor_now(now)));
    let summary = if drain { drain_channel(link, channel, now).await? } else { peek_channel(link, channel, now).await? };
    report.note(format!("channel {channel:02x}: {summary}"));
    shared.with(|r| r.channels.push((channel, summary)));
    Ok(())
}

/// Opens a sync and records what arrives without acknowledging anything, so the ring keeps it.
async fn peek_channel(link: &mut Link, channel: u8, now: i64) -> Result<String> {
    link.send(&cmd::sync_open(protocol::cursor_now(now), channel)).await?;
    let Some(reply) = link.wait_for(&[0x82], Duration::from_secs(4)).await else {
        return Ok("no 82 reply".into());
    };
    let reply_hex = protocol::hex(&reply);
    if reply.get(1) == Some(&0xff) {
        return Ok(format!("{reply_hex} (nothing to drain)"));
    }
    link.send(&cmd::FETCH).await?;
    let mut pages = [0usize; 3];
    let mut ended = false;
    for f in link.collect(Duration::from_secs(3), Duration::from_secs(8)).await {
        match f[0] {
            0x4c => pages[0] += 1,
            0x4d => pages[1] += 1,
            0x47 => pages[2] += 1,
            0x50 => ended = true,
            _ => {}
        }
    }
    Ok(format!(
        "{reply_hex}; {} epoch, {} sport, {} optical page(s) seen, none acknowledged; {}",
        pages[0],
        pages[1],
        pages[2],
        if ended { "ended with 0x50" } else { "no 0x50" }
    ))
}

/// Drains with the same state machine as Ringmaster.
async fn drain_channel(link: &mut Link, channel: u8, now: i64) -> Result<String> {
    let id = match channel {
        CHANNEL_SPORT => Channel::Sport,
        CHANNEL_ALL_DAY => Channel::AllDay,
        _ => Channel::Sleep,
    };
    let (mut machine, mut actions) = ChannelDrain::start(id, now);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20 * 60);
    let (mut reply, mut outcome, mut pages) = (None, None, [0usize; 3]);
    loop {
        for action in actions.drain(..) {
            match action {
                // Frames are flushed to the report file as they arrive, before this point.
                Action::Store(batch) => match batch {
                    Batch::Epochs(_) => pages[0] += 1,
                    Batch::Sport(_) => pages[1] += 1,
                    Batch::Optical(_) => pages[2] += 1,
                    Batch::EventLog(_) => {}
                },
                Action::Send { bytes, after } => {
                    tokio::time::sleep(after).await;
                    link.send(&bytes).await?;
                }
                Action::Progress { .. } => {}
                Action::Finished(o) => outcome = Some(o),
            }
        }
        if machine.is_done() || tokio::time::Instant::now() >= deadline {
            break;
        }
        actions = match link.recv(Duration::from_secs(4)).await {
            Some(f) => {
                if f.first() == Some(&0x82) && reply.is_none() {
                    reply = Some(protocol::hex(&f));
                }
                machine.on_frame(&f)
            }
            None => machine.on_quiet(),
        };
    }
    Ok(format!(
        "{}; {} epoch, {} sport, {} optical page(s) acknowledged; outcome {}",
        reply.unwrap_or_else(|| "no 82 reply".into()),
        pages[0],
        pages[1],
        pages[2],
        outcome.map(|o| format!("{o:?}")).unwrap_or_else(|| "none (20 min limit)".into())
    ))
}

fn local_time(unix: i64) -> String {
    Local.timestamp_opt(unix, 0).single().map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_else(|| "?".into())
}

fn findings(opts: &Options, report: &Report, shared: &Shared, error: Option<String>) {
    let frames = report.frames();
    let r = shared.results.lock().unwrap();
    let missing = |v: &Option<String>| v.clone().unwrap_or_else(|| "not reached".into());

    if let Some(e) = &error {
        report.finding("Run", format!("STOPPED EARLY: {e}"));
    }
    if opts.rwfit {
        return;
    }
    report.finding("1. GATT layout matches section 2?", missing(&r.gatt));
    report.finding("2. BlueZ pairing (Just Works) succeeds?", missing(&r.pairing));
    report.finding("3. SM3 authentication accepted?", {
        let mut a = missing(&r.auth);
        if r.wake_fetch == Some(true) || r.wake_query == Some(true) {
            a.push_str("; data commands answered afterwards, so authentication worked");
        } else if r.wake_fetch.is_some() {
            a.push_str("; NO data command answered afterwards (bond or authentication failed)");
        }
        a
    });
    let yes_no = |v: Option<bool>| match v {
        Some(true) => "answered",
        Some(false) => "no answer",
        None => "not tried",
    };
    report.finding(
        "4. Which request wakes telemetry?",
        format!("07 00 00 -> 0x87: {}; d0 00 00 -> 0x10: {}", yes_no(r.wake_fetch), yes_no(r.wake_query)),
    );

    if opts.overnight.is_some() || opts.explore.is_some() || opts.osa.is_some() || opts.ppg.is_some() {
        report.finding("Device", missing(&r.identity));
        report.finding("Environment", r.environment.join("\n"));
        return;
    }
    let descriptors: Vec<(f64, Descriptor)> = frames.iter().filter_map(|(t, f)| Descriptor::parse(f).map(|d| (*t, d))).collect();
    report.finding("5. Descriptor offsets plausible?", descriptor_finding(&descriptors));
    let mut dlines = vec!["time      op  batt  state     steps  temp1  temp2  mV     case            raw".to_string()];
    for (t, f) in frames.iter().filter(|(_, f)| matches!(f.first(), Some(0x10 | 0x87))) {
        if let Some(d) = Descriptor::parse(f) {
            dlines.push(format!(
                "{t:8.1}  {:02x}  {:>3}%  {:02x} {:<6} {:>5}  {:>5.1}  {:>5.1}  {:>5}  {:<15} {}",
                f[0],
                d.battery,
                d.state,
                protocol::state_name(&d),
                d.steps,
                d.skin_temp[0],
                d.skin_temp[1],
                d.millivolts,
                protocol::case_text(&d),
                protocol::hex(f)
            ));
        }
    }
    report.section("Status descriptors", dlines);

    let sport = r.channels.iter().find(|(c, _)| *c == protocol::CHANNEL_SPORT).map(|(_, s)| s.clone());
    report.finding("6. Sport channel (02) exists on this ring?", sport.unwrap_or_else(|| "not reached".into()));

    report.finding("7. Which epoch dates records correctly?", epoch_finding(&frames, r.sync_unix, opts.drain));
    report.section("History records", history_lines(&frames));

    let mut channels = String::new();
    for (c, s) in &r.channels {
        channels.push_str(&format!("channel {c:02x}: {s}\n"));
    }
    report.finding("8. Sync-open replies", if channels.is_empty() { "not reached".into() } else { channels.trim_end().to_string() });

    if !r.live_test.is_empty() {
        report.finding("Live measurement start-up methods", r.live_test.join("\n"));
    }
    report.finding("Live heart rate", r.live.clone().unwrap_or_else(|| if opts.live { "not reached".into() } else { "skipped".into() }));
    report.finding("Device", missing(&r.identity));
    report.finding("Environment", r.environment.join("\n"));
}

fn descriptor_finding(ds: &[(f64, Descriptor)]) -> String {
    if ds.is_empty() {
        return "no descriptors received".into();
    }
    let range = |f: &dyn Fn(&Descriptor) -> f32| {
        let v: Vec<f32> = ds.iter().map(|(_, d)| f(d)).collect();
        (v.iter().cloned().fold(f32::MAX, f32::min), v.iter().cloned().fold(f32::MIN, f32::max))
    };
    let (b0, b1) = range(&|d| d.battery as f32);
    let (t0, _) = range(&|d| d.skin_temp[0].min(d.skin_temp[1]));
    let (_, u1) = range(&|d| d.skin_temp[0].max(d.skin_temp[1]));
    let (m0, m1) = range(&|d| d.millivolts as f32);
    let mut issues = Vec::new();
    if b1 > 100.0 {
        issues.push("battery above 100");
    }
    if t0 < 15.0 || u1 > 45.0 {
        issues.push("temperature outside 15..45 degC");
    }
    if m0 < 3000.0 || m1 > 4500.0 {
        issues.push("voltage outside 3000..4500 mV");
    }
    let mut states: Vec<String> = ds.iter().map(|(_, d)| format!("{:02x}", d.state)).collect();
    states.sort();
    states.dedup();
    let gaps: Vec<f64> = ds.windows(2).map(|w| w[1].0 - w[0].0).collect();
    let cadence = if gaps.is_empty() {
        "n/a".into()
    } else {
        format!("{:.0}..{:.0} s", gaps.iter().cloned().fold(f64::MAX, f64::min), gaps.iter().cloned().fold(0.0, f64::max))
    };
    format!(
        "{}: {} frames; battery {b0}..{b1}%, temps {t0:.1}..{u1:.1} degC, {m0}..{m1} mV, states {}, spacing {cadence}",
        if issues.is_empty() { "PLAUSIBLE" } else { "CHECK" },
        ds.len(),
        states.join("/"),
    ) + &if issues.is_empty() { String::new() } else { format!(" ({})", issues.join(", ")) }
}

fn epoch_finding(frames: &[(f64, Vec<u8>)], sync_unix: Option<i64>, drain: bool) -> String {
    let counters: Vec<u32> =
        frames.iter().filter(|(_, f)| f.first() == Some(&0x4c)).flat_map(|(_, f)| protocol::counters(f, protocol::EPOCH_LEN)).collect();
    let (Some(&newest), Some(&oldest), Some(sync)) = (counters.iter().max(), counters.iter().min(), sync_unix) else {
        return "no epoch records received".into();
    };
    let line = |name: &str, epoch: i64| {
        let n = epoch + newest as i64;
        let o = epoch + oldest as i64;
        let delta = (n - sync) as f64 / 60.0;
        let verdict = if delta > 2.0 { "IMPOSSIBLE (newest record after the sync)" } else { "possible" };
        format!("{name} {epoch}: records {} .. {}; newest is {delta:+.1} min from sync, {verdict}", local_time(o), local_time(n))
    };
    let mut out = format!("{}\n{}\nsync opened {}", line("A", protocol::EPOCH_A), line("B", protocol::EPOCH_B), local_time(sync));
    if !drain {
        out.push_str("\npeek mode shows the oldest pending records only, so only the IMPOSSIBLE test is decisive");
    }
    out
}

fn history_lines(frames: &[(f64, Vec<u8>)]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut epochs = Vec::new();
    let (mut sport, mut optical) = (0, 0);
    for (_, f) in frames {
        match f.first() {
            Some(0x4c) => epochs.extend(protocol::epochs(f)),
            Some(0x4d) => sport += protocol::page_records(f, protocol::SPORT_LEN).len(),
            Some(0x47) => optical += protocol::page_records(f, protocol::OPTICAL_LEN).len(),
            _ => {}
        }
    }
    let count = |l: EpochLayout| epochs.iter().filter(|e| e.layout() == l).count();
    lines.push(format!(
        "{} epoch records ({} idle, {} activity, {} sleep-vitals); {sport} sport records; {optical} optical records",
        epochs.len(),
        count(EpochLayout::Idle),
        count(EpochLayout::Activity),
        count(EpochLayout::SleepVitals)
    ));
    if epochs.is_empty() {
        return lines;
    }
    lines.push(String::new());
    lines.push("counter   time (epoch A)        layout        hr  hrv  conf  resp  b8".into());
    let shown: Vec<_> =
        if epochs.len() > 60 { epochs[..30].iter().chain(epochs[epochs.len() - 30..].iter()).collect() } else { epochs.iter().collect() };
    for (i, e) in shown.iter().enumerate() {
        if epochs.len() > 60 && i == 30 {
            lines.push(format!("... {} more ...", epochs.len() - 60));
        }
        lines.push(format!(
            "{:08x}  {}  {:<12} {:>3}  {:>3}  {:>4}  {:>4.1}  {:02x}",
            e.counter,
            local_time(protocol::EPOCH_A + e.counter as i64),
            format!("{:?}", e.layout()),
            e.raw[4],
            e.raw[5],
            e.raw[6],
            e.raw[7] as f32 / 8.0,
            e.raw[8]
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_xor(mut f: Vec<u8>) -> Vec<u8> {
        f.push(ringconn::frame::xor(&f));
        f
    }

    #[test]
    fn analysis_of_synthetic_frames() {
        // battery 76 %, idle, 12 steps, 34.5 / 33.8 degC, 3950 mV, not in case
        let d = with_xor(vec![0x87, 76, 0x03, 0, 0, 12, 0x01, 0x59, 0x01, 0x52, 0, 0, 0, 0, 0x0f, 0x6e, 0, 0xff]);
        assert_eq!(d.len(), 19);
        let parsed = Descriptor::parse(&d).unwrap();
        assert_eq!((parsed.battery, parsed.steps, parsed.millivolts), (76, 12, 3950));
        let ds = vec![(1.0, parsed.clone()), (40.0, parsed)];
        assert!(descriptor_finding(&ds).starts_with("PLAUSIBLE"));

        let sync = 1_792_000_000i64;
        let counter = (sync - protocol::EPOCH_A - 300) as u32;
        let mut page = vec![0x4c, 0x00, 0x00];
        page.extend(counter.to_be_bytes());
        page.extend([62, 40, 9, 120, 97, 0x0a, 3, 2, 1, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0]);
        let page = with_xor(page);
        let frames = vec![(5.0, d), (6.0, page)];
        let f = epoch_finding(&frames, Some(sync), true);
        assert!(f.contains("A 1577793600") && f.contains("-5.0 min from sync, possible"), "{f}");
        assert!(f.contains("+235.0 min from sync, IMPOSSIBLE"), "{f}");
        let lines = history_lines(&frames);
        assert!(lines[0].starts_with("1 epoch records (0 idle, 0 activity, 1 sleep-vitals)"), "{lines:?}");
    }
}
