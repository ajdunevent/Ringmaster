# Ultrahuman Ring AIR

Protocol notes gathered for a possible `crates/ultrahuman`. Nothing here has been tried with
Ringmaster yet. Source: Gadgetbridge's experimental Ultrahuman driver (codeberg.org/Freeyourgadget/Gadgetbridge,
`devices/ultrahuman`, `service/devices/ultrahuman`, read 2026-10-27). AGPL, so facts only; no code.
Gadgetbridge's authors mark it experimental and partly unconfirmed.

## Finding the ring

- Advertised name `UH_` followed by 12 hex digits (the Bluetooth address).
- Gadgetbridge pairs it without the vendor app and without a vendor account. It does not ask for
  an unpair first. On firmware 02.00.10.42 the ring needs a bond for its response
  characteristic: LE Secure Connections Just Works, approved through a pairing agent (see the
  probe runs below).

## Services

All share the base `86f6xxxx-f706-58a0-95b2-1fb9261e4dc7`.

| Service | Characteristic | Use |
|---|---|---|
| `86f65000` | `86f65001` | Commands (write) |
| `86f65000` | `86f65002` | Command responses (notify) |
| `86f61000` | `86f61001` | Device state (read, notify), 7 bytes |
| `86f66000` | `86f66001` | Live exercise and breathing data (notify) |
| `86f63000` | `86f63001` | Unknown (notify); Gadgetbridge only logs it |

Device Information (`180a`) gives hardware, firmware and serial. Reading characteristics the ring
lacks has caused BLE failures on Android, so only these three are read.

## Commands

A command is the operation byte followed by its arguments, with no checksum. A response starts
with the operation, a status byte (`00` success, `ff` refused) and a result byte (`01` success).
Responses end in a CRC-16/CCITT-FALSE (polynomial `0x1021`, initial value `0xffff`) of the bytes
after those three, little-endian; it held for every response of a probe run, refusals included.

| Op | Arguments | Meaning |
|---|---|---|
| `02` | Unix time, u32 LE | Set the clock (UTC seconds, unlike Colmi's local BCD) |
| `07` | none | First stored record number: response bytes 3 to 4, u16 LE |
| `08` | none | Last stored record number: same layout |
| `04` | record number, u16 LE | Records from that number on |
| `59` | none | Check data; answers with the current exercise state in byte 3 |
| `62` / `64` | none | Start / stop an exercise (live heart rate on the data characteristic) |
| `72` / `74` | none | Start / stop a breathing session |
| `70` | none | Airplane mode; result `02` on the charger, `03` battery too full |
| `d1` / `d2` | none | All-day SpO2 on / off |
| `98` | none | Factory reset |

Gadgetbridge's connect sequence: read hardware, firmware, serial and state; enable notifications on
all four; send `59`; set the time; set SpO2.

## History records (`04`)

Response: `04 status count`, then `count` records of 32 bytes (up to 7 per response). Status `ee`
means nothing recorded. Gadgetbridge sends `04` once and stops when the last record number is
reached or a response holds fewer than 7 records, so the ring appears to keep sending responses
until it reaches the end; confirmed for 23 records in four responses, after which the ring sends
`04 ee 00 ff ff`. Record numbers wrap: a first number above the last means start from 0.

Record layout (offsets within the 32 bytes, little-endian):

| Offset | Size | Field |
|---|---|---|
| 0 | u32 | Time of the optical readings, Unix seconds |
| 4 | u8 | Heart rate, 0 for none |
| 5 | u8 | HRV, 0 for none |
| 6 | u8 | SpO2, 0 for none |
| 7 | u8 | Record kind: 100 and above not worn; during an exercise one record every 2 s, kind 5 with a reading and 10 without; 1 carries the latest optical reading and opens each exercise; 7, 10 and 12 also as 5-minute records without readings (Gadgetbridge: 5 breathing, 6 exercise) |
| 8 | u32 | Time of the temperature readings |
| 12 | f32 | Highest skin temperature, °C |
| 16 | f32 | Lowest skin temperature, °C |
| 20 | u32 | Time of the activity readings |
| 24 | u8 | Movement intensity (Gadgetbridge scales by 1/150) |
| 26 | u16 | Steps |
| 28 | u8 | Stress, 0 to 255 (Gadgetbridge scales to 0 to 100) |
| 30 | u16 | Record number |

Bytes 25 and 29 were 0 in every record seen. Heart rate, SpO2, skin temperature, intensity, steps,
stress and the record number are confirmed against real readings (2026-11-14). Times before
2020 count seconds from when the ring started, as stamped before its clock was set; counting
back 300 s per record number from the next dated record gives their real times.

Records are not deleted on read, so syncing again is safe, and a
sync can resume from the last record number stored.

## State (`86f61001`)

7 bytes: battery percent in byte 0, state in byte 5 (`00` normal, `03` charging), ring temperature
in byte 6. Bytes 1 to 4 are a signed 32-bit little-endian value of unknown meaning: 4,698 to
14,097 on the charger on two rings, and 585 or -585 off it on both.

The battery percent cannot be trusted on the charger. A ring that read 9 % off the charger read
100 % after about 40 minutes on it, 22 % seconds after coming off, and 6 % ten minutes later.
Ringmaster should show "Charging" while byte 5 is `03`, and a level only from readings off the
charger.

## Live data (`86f66001`)

- Exercise, 11 bytes: `62 06 00`, time (u32 at byte 3), heart rate (byte 7), a status (byte 8:
  `05` with a reading, `0a` with heart rate 0 from a ring off the finger), and the response check
  (CRC over bytes 3 to 8). Confirmed 2026-11-14: frames every
  2 s from about 12 s after exercise start, the first reading 75 bpm settling to 60. Exercise
  start answers `62 00 01 62` off the charger and `62 ff 01 11` (refused) on it.
- Breathing, 27 bytes: op `72`, time (u32 at 3), heart rate (byte 7), HRV (byte 9), temperature
  (f32 at 11).

## Not covered by Gadgetbridge

Sleep and sleep stages (the vendor computes them in its cloud), a battery request (state
notifications give it), firmware updates. Ringmaster's own sleep estimate would have to work from
heart rate, HRV, intensity and temperature, as it does for RingConn.

## Probing a ring

`cargo run --release -p ringprobe -- --ultrahuman` (quit Ringmaster first; wear the ring for the
live part, or add `--skip-live`). It reads the state, tries commands without a bond and pairs only
if the ring stays silent, sets the clock, reads the first and last record numbers and every
stored record, starts an exercise for 30 s for live heart rate and stops it, then listens for
20 s. The findings answer what Gadgetbridge leaves open: whether a bond is needed, whether one
`04` request returns every record, how far apart records are, which check the last two bytes of
a response are, and what the live data and `86f63001` carry. It never sends SpO2, airplane mode
or factory reset, and records stay on the ring.

`crates/ultrahuman` holds the protocol for both the probe and, later, the app.

### First runs (2026-11-13, Ring AIR, firmware 02.00.10.42, hardware R1.01.01.01)

- On its charger the ring was found but refused the connection (`le-connection-abort-by-local`).
  Off the charger it connected and its services were readable in 2 s.
- Reads work without a bond: GAP and Device Information (manufacturer "Ultrahuman Healthcare Pvt
  Ltd", model `R1.00.00.01`, firmware `02.00.10.42`, hardware `R1.01.01.01`).
- Without a bond, the ring enabled notifications on `86f61001`, `86f66001` and `86f63001` at once
  but left the request for `86f65002`, its response characteristic, unanswered (seen twice: once
  until D-Bus gave up after 2 minutes, once for the probe's 15 s limit), and the link dropped
  soon after. That fits a characteristic that needs an encrypted link. The probe now pairs before
  enabling any notifications.
- Manufacturer data (company `0x2d42`): `52 54 31 33 5c 01 02 72 00` on the charger and
  `52 54 31 33 5c 01 01 72 00` off it, then `52 54 31 33 04 00 01 eb 00` the next morning.
  `52 54 31 33` is "RT13", as in the serial number. Byte 6 may be the charging state; byte 4
  was 92, then 4, then 0 while the state characteristic read 9 % in the same run, so it is not
  the battery level as such.
- The state characteristic read `09 49 02 00 00 00 1d` off the charger: battery 9 %, state `00`,
  ring 29 °C, matching the layout below.
- A btmon capture (on the charger) showed why. The ring answers the write that enables
  notifications on `86f65002` (its CCCD, handle `0x0019`) with ATT error `05`, Insufficient
  Authentication, so response notifications need a bond; the other three notifying
  characteristics do not. Asked to pair, the ring answers with LE Secure Connections, IO
  capability NoInputNoOutput, bonding, and no keys to distribute (Pairing Response
  `03 00 09 10 00 00`). BlueZ then asks an agent to confirm the Just Works pairing, and with none
  registered it refuses ("No agent available for request type 2"), which reached the probe as
  "Authentication Failed". The probe now registers an agent that confirms pairing with this ring
  only. Ringmaster will need the same before it can use the ring.
- Advertisement byte 4 tracks the battery loosely (`64` with the state at 100 %, but `00` at 9 %);
  byte 5 was `01` on the charger and `00` off it.
- **First full run** (2026-11-14, on the charger, after a factory reset): with the agent
  registered, BlueZ made the bond by itself when the ring refused the notification request, and
  every command was answered. The set-time response echoes the time sent. First and last record
  numbers were 1 and 7, from 09:46:56, so the factory reset had restarted the numbering. One `04`
  request returned all seven records in one response. Records are 300 s apart, and their three
  times are equal. Six were kind 110 (not worn); one was kind 12 with intensity 36, a kind
  Gadgetbridge does not name. Exercise start was refused on the charger (`62 ff 01 11`), and
  exercise stop answered `64 ff 01 ff`.
- **Second run** (ten minutes later, worn, off the charger): no new bond needed. The ring's clock
  had run 2 s fast since the previous run, and the probe's set time moved the record times back
  by 2 s. Records 8 and 9 were kinds 10 and 7; record 9 had intensity 150 and 21 steps. No record
  yet had heart rate, HRV, SpO2 or skin temperature, so those fields are still unchecked against
  real values. 8 of 8 responses passed the check.
- **A second Ring AIR** (2026-11-14, firmware 02.00.08.53, same hardware, on its charger, never
  reset): it also needed a bond for response notifications, and `Pair()` succeeded in 6.1 s
  through the agent. Its record numbers also started at 1, at 10:33:59, minutes after it was put
  on the charger. Its advertisement was shorter: BlueZ showed key `0x2d31` with
  `52 54 31 33 64`, against `0x2d42` and nine bytes on the first ring, so the "company" key is
  part of the data and not a company. After the last records response the ring sent
  `04 ee 00 ff ff` (status `ee`, nothing more; the check of an empty body is `ffff`). Exercise start
  was refused on the charger, as on the first ring. The current field ran 8,811 to 11,160 while
  charging.
- **The second ring off its charger** (ten minutes later): battery 99 %, current 0. Exercise start
  was accepted and live heart rate ran 69 down to 55. Its records 1 and 2, read as 10:33:59 and
  10:38:59 in the previous run, now read as 309 and 609: seconds since the ring started, as
  stamped before its clock was set. Record 3 (10:43:59) was stamped after the previous run set
  the clock. Counting back 300 s per record number from record 3 gives exactly the times read in
  the previous run, so the ring itself sometimes converts these times when they are read and
  sometimes does not. `date_undated` in `crates/ultrahuman` does that conversion for every read,
  and the app should apply it before storing records.
- **Exercises are kept in history.** Each probe's 30 s exercise left one record every 2 s, kind 5,
  whose heart rates equal the live frames of that exercise exactly (75, 71, 68 ... 60 on the first
  ring; 69, 66 ... 55 on the second). That confirms bytes 4 (heart rate) and 7 (kind) and places
  skin temperature at bytes 12 and 16 (f32, 33.5 to 34.2 °C once warm, about 31 °C in the first
  record). On firmware 02.00.10.42 these records carry SpO2 (97 to 99); on 02.00.08.53 they carry
  none, and HRV appears from the seventh record (28 to 58 ms) with byte 28 (stress) equal to it.
  The 5-minute grid starts again after an exercise. Gadgetbridge names kind 5 "breathing" and 6
  "exercise"; both firmware versions wrote 5 for an exercise.
- An ordinary worn record (kind 1, first ring, 10:34) had heart rate 111, HRV 128, SpO2 99,
  intensity 150 and stress 221, with its optical time 144 s before its other two times. Kinds 7,
  10 and 12 came from a worn ring with no readings; kind 100 from the second ring just after it
  came off the finger.
- Both rings' batteries fall quickly. The second ring read 99 % just off the charger, then 40 %,
  26 % and 5 % over about 20 minutes, and dropped the link during the next exercise. The first
  read 63 %, 60 %, 55 %, 54 % and 49 % over three minutes of probing.
- An exercise run with the ring off the finger (live status `0a`) still wrote a record every 2 s,
  as kind 10 with no readings. Exercises are told apart by the 2 s spacing, not the kind alone.
- The state's bytes 1 to 4 read 0, 585, -585 and 1,173 off the charger.
- On the charger the state was notified about every 5 s (bytes 1 and 2 changing: `11 37`,
  `2d 30`, `c5 34`; meaning unknown). Taken off the charger, the next state read battery 22 %.
- One service is not in Gadgetbridge's notes: `8d53dc1d-1db7-4cd3-868b-8a527460aa84` with
  `da2e7828-fbce-4e01-ae9e-261174997c48` (write without response, notify). Its role is unknown;
  the probe leaves it alone.

## Fit with Ringmaster

Supported since 2026-11-14 (`crates/ringmaster/src/backend/ultrahuman_ring.rs`):

- **Recognition and pairing**: by the `UH_` name or the command service; the app's pairing agent
  confirms the Just Works bond the response characteristic needs.
- **On connect**: sets the clock, reads the state once, then hears it notified. While the ring is
  charging the battery shows as "Charging" with no level.
- **Sync**: sets the clock again, reads the first and last record numbers, and reads from the
  last record stored. That record is read again and its time compared, so a ring whose numbers
  started over (factory reset, flat battery) is read in full. Records stamped before the clock
  was set are dated by `date_undated`; records still undated, or in the future, are left out.
  Worn records give heart rate, HRV, SpO2 (optical time), skin temperature (highest, at the
  temperature time), stress, intensity and steps (activity time) in `samples` and `steps`.
  Every dated record is also kept as sent in `ultrahuman_records`, for decoding and sleep work.
- **Quick reading**: heart rate through an exercise (`62` / `64`); the ring keeps the exercise in
  its history, so the next sync stores its 2-second readings too. There is no live SpO2.
- **Sleep**: none yet. The Sleep tab stays empty for these rings until an estimate from the
  stored records is built and checked against real nights.
