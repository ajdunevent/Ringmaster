# ringprobe

A development tool, not part of Ringmaster. It connects to a ring, records everything the ring
says in a plain-text report, and runs the experiments behind the findings in `docs/PROTOCOL.md`.
Use it to check a ring Ringmaster does not yet support, to repeat a finding, or to collect data
for a bug report.

## Before running

- Quit Ringmaster (menu, Quit) so the two do not talk to the ring at once.
- Turn off Bluetooth on any other phone that runs the ring's official app, so it does not connect
  during the run.
- Take the ring out of its case so it advertises, and wear it for the live reading.
- To test pairing from scratch, remove the ring first with `bluetoothctl remove <address>`. If it
  is already bonded, the report says so and skips the pairing test.

## Running it

From the project root:

```sh
cargo run --release -p ringprobe -- [options]
cargo run --release -p ringprobe -- --help
```

A plain run scans, pairs, authenticates, reads the ring's identity and status, looks at its
history without removing any of it, listens for a minute and takes a live heart-rate reading
(wear the ring for that). The report is written to `ringprobe-<date>-<time>.txt` in the current
folder; an existing file is never overwritten. Ctrl-C stops a run early and still writes the
report.

## Safe options

| Option | What it does |
|---|---|
| (none) | The plain run above |
| `--address XX:XX:XX:XX:XX:XX` | Use this ring rather than the bonded one or the first found |
| `--listen SECONDS` | Length of the passive listen (default 60) |
| `--skip-live` | Leave out the live heart-rate reading |
| `--live-test` | Compare ways of starting a live reading (about 4 minutes) |
| `--out FILE` | Report path |

## Options that need `--risky`

These change the ring's state or its stored data. ringprobe refuses them unless `--risky` is
also given.

| Option | Risk |
|---|---|
| `--drain` | Downloads history and acknowledges every page, so the ring deletes it. Any other app syncing the ring will not receive that data; Ringmaster can import it from the report (below). |
| `--overnight HOURS` | Back-to-back live sessions. The ring keeps none of its own 2.5-minute records while they run, so that time is missing from its history. Ends with a short factory-mode PPG test. |
| `--overnight HOURS --raw-ppg` | Raw PPG in factory mode all night. Same gap in the ring's own records. Chained factory-mode sessions have left a ring answering no command until it was seated in its case. |
| `--ppg SECONDS` | Raw PPG in factory mode, restarted whenever it stops. Same lock-up risk. |
| `--explore MINUTES` | Tries values of known commands that are not yet confirmed, factory sensor tests and apnea work mode. |
| `--osa MINUTES` | Switches the ring's apnea monitoring on, then off again. |
| `--rwfit` | For RWfit rings. Reading their history may remove it from the ring. |

ringprobe sends only commands found in the official apps or in published protocol notes, never
writes to the RingConn's secondary (probably firmware update) service, and leaves factory mode
with `20 01 00` after each session. Sweeping unknown command bytes has
bricked a ring before (`docs/PROTOCOL.md`, section 1); do not add an option that does.

### If the ring stops answering

A ring that connects but answers nothing has come back every time so far once it was seated in
its charging case. If it still does not answer, restart it by putting it into the case 5 times
quickly within 10 seconds. Then sync in Ringmaster.

## Reports

A report holds the ring's Bluetooth address and health readings. Remove the address before
sharing one publicly.

Ringmaster can copy the history pages in a report into its own database:

```sh
ringmaster --import-probe-report ringprobe-20261021-1311.txt
```

Raw PPG runs also write `<report>.ppg.csv`, about 4 MB an hour. Both file patterns are in
`.gitignore`.
