# Ringmaster backlog

Items deferred on purpose, with what they wait for. Updated 2026-11-06.

## v0.1.0 (current)

- Publish on the project's forge (https://dev.dunevent.net/aj/Ringmaster). Not a Flathub target, so
  screenshots are not needed. metainfo has the URLs and the 0.1.0 notes (2026-11-06); AppStream
  allows one URL per type, so the metainfo lists Liberapay and the README lists PayPal as well.
- Confirmed in daily use by 2026-11-06: connection, overnight and morning syncs, reconnection and
  battery (80 % after at least 36 hours). The link watchdog (delta 33), the stale notification
  session fix (delta 35) and 15-minute syncs left no gaps.

## After v0.1.0: app

- **Tabs** (done 2026-11-06): Latest, History, Sleep (nights against the goal over 2 or 4 weeks,
  averages, a list opening each night in History) and Activity (daily steps the same way). Next for
  Sleep: restful and restless periods across a night, oxygen 3 or more points below the night's
  usual, stages and apnea when ready. Sleep duration stays time asleep (the window less restless
  epochs).
- **Workouts**: the Gen 2 sport channel `02` is unconfirmed and the official app cannot be
  captured (it needs SafetyNet), so detection from heart rate and movement is the route; it needs
  labelled workouts to tune against before it goes on the Activity tab.
- **CSV export.**
- **Revisit with more nights**: skin temperature falling on getting up as a wake-time signal (93.7
  to 70.2 °F between 06:02 and 06:17 on 2026-10-22); brief confidence dips between 04:30 and 05:45
  as short wakings.

## After v0.1.0: research

### Raw PPG (PROTOCOL sections 14 and following)

- Works in factory mode (work mode 2): `22 08 00` streams four channels at 25 Hz, `23 03 00` the
  accelerometer, `20 01 00` leaves. `ringprobe --ppg` and `--overnight HOURS --raw-ppg` record it.
- Sessions chained without leaving factory mode locked the ring twice (at 5 and 11 minutes) until
  it was put in its case, but ran 21.6 minutes cleanly once. Leaving with `20 01 00` and resting 5 s
  after each session has run an hour by day and 97 minutes by night without a lock-up, with single
  sessions of up to 40 minutes.
- **The ring keeps none of its own 2.5-minute records while PPG streams** (2026-10-28 night): raw
  PPG replaces the ring's measurements for that time. Options: short bursts at night, heart rate
  and HRV of our own from PPG, or a SleepPPG-Net style stager (needs labelled nights).
- The probe stops after 20 minutes of connections the ring takes but does not answer.

### Apnea

- An "apnea monitoring" setting: ODI from the live SpO2 stream and ln(a/b) (which tracks SpO2,
  r 0.85 to 0.95), CVHR from raw PPG, following published definitions, with live data in its own
  table. Live sessions also stop the ring's own records.
- The ring's own apnea paths: parameter `05 22` is acknowledged but did nothing on FR02.018;
  replies `45` (offline apnea), `48` (automatic), `46` (online, acknowledged with `c6`); `0a`
  (offline download) is unanswered so far. Decode `0a`/`8a` and try it in factory mode.

### Sleep stages

- The rule-based stager agrees with the official app's 158 imported nights 80 % of the time
  (kappa 0.68). Which early blocks are deep is unexplained by stored data; skin temperature is the
  main suspect, and the imported nights have none.
- The official app 4.x stages on-device with an ExecuTorch model taking raw PPG in the published
  SleepPPG-Net format (1200 windows of 1024 samples at 34.13 Hz, four classes). The model is the
  vendor's and is not used; a model of our own needs raw PPG nights with labels.

### Firmware updates

- Flagged 2026-10-27. Needs how the app fetches images and the BLE transfer (`09`/`89` in 3.10.0).
  A failed update can brick a ring: a confirmed protocol and a recovery path come before any
  attempt.

## Other rings

- **Other RingConn models** (Gen 1, Gen 2 Air, Gen 3): OpenCircuit and RingLink cover parts of the
  Gen 3 protocol. Device Information tells generations apart; `ringconn` would need per-model
  record layouts.
- **Colmi** (`crates/colmi`, `docs/COLMI.md`): written from Gadgetbridge and tahnok's notes,
  storage brand-neutral, discovery fixed for names that arrive late (delta 65). Untested on a ring:
  the "R12" tested in 2026-11 is an RWfit ring.
- **RWfit** (`crates/rwfit`, `docs/RWFIT.md`, `ringprobe --rwfit`): the R12 advertises as "SR16"
  with company `0x06D6` and has HID over GATT (`1812`), the likely route for its gestures, and
  `FF00`, which points to the JieLi framing. **Blocked**: BlueZ closes its ATT channel when the ring
  sends a second request before the first is answered, and the kernel drops the link 2 to 6 s
  later, on the phone and on a desktop (`docs/bluez/`). Patching BlueZ is ruled out. Options: a
  report to the BlueZ developers and FuriLabs (capture and debug log in hand), or a raw ATT socket
  of Ringmaster's own, uncertain to work.
- **Ultrahuman** (`docs/ULTRAHUMAN.md`): a `crates/ultrahuman` in the Colmi
  pattern; 32-byte history records with heart rate, HRV, SpO2, skin temperature, steps and stress,
  no sleep from the ring.

## Official app

- Reading the APK for command bytes and record formats is for interoperability; no vendor code or
  model weights are used. 3.10.0 is the newest build Blutter reads (3.11.0 and later ship
  Shorebird's private Dart fork); findings are in PROTOCOL section 14.
- 4.4.x (strings and assets only): the ring can deliver PPG in bulk (`BleCompressPpgRsp`,
  `BleCompressPpgProgress`, `BleDataUncompress`, `CompressPPG`, a `PpgSample` table), and "Lab
  Sleep Pro" has a bedtime schedule (`sleepProjectStart`, `sleepProjectEnd`, `sleepProjectSwitch`):
  the ring likely records PPG over a set window and the app pulls it compressed, which would avoid
  live streaming at night. Apnea names: `BleAutoOsaDataRsp`, `BleGetOfflineOsaRsp`,
  `BleOfflineOsaDataRsp`, `BleOnlineOsaDataRsp`, `BleOsaCompatibleMode`, `OsaPPGV2Bean`.
  `ppg_calib.csv` beside the sleep model (40,705 rows in blocks of 35) is not decoded.
- 2.2.1: the `12` PPG frame handler scales by a per-ring constant between 700,000 and 1,000,000
  (default 773,333) and reads two 4-bit gain codes (5 means x50, 2 means x5, else x1).
- The imported `official` table holds the app's sleep stages, naps and apnea reports for 2025-12 to
  2026-06, for comparing future algorithms. A data request to RingConn could add skin temperature.
- `debug_trace.txt` from the app: `10 07`/`10 08` pairs (median 30 minutes apart, about 6 a day,
  mostly 17:00 to 03:00) and `15 31` followed by `15 12` (median 14 minutes, mostly overnight) are
  still unnamed; compare with Ringmaster's own `event_log`.
- The app's logs (`files/log`, `files/onlineLog`) are encrypted.
