# Ultrahuman Ring AIR

Protocol notes gathered for a possible `crates/ultrahuman`. Nothing here has been tried with
Ringmaster yet. Source: Gadgetbridge's experimental Ultrahuman driver (codeberg.org/Freeyourgadget/Gadgetbridge,
`devices/ultrahuman`, `service/devices/ultrahuman`, read 2026-10-27). AGPL, so facts only; no code.
Gadgetbridge's authors mark it experimental and partly unconfirmed.

## Finding the ring

- Advertised name `UH_` followed by 12 hex digits (the Bluetooth address).
- Gadgetbridge pairs it without the vendor app and without a vendor account. It does not ask for
  an unpair first. Whether the ring needs a bond is not stated; try without first, as with Colmi.

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
with the operation, a status byte (`00` success) and a result byte (`01` success). Responses end in
two bytes that look like a checksum of unknown algorithm.

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
until it reaches the end. Record numbers wrap: a first number above the last means start from 0.

Record layout (offsets within the 32 bytes, little-endian):

| Offset | Size | Field |
|---|---|---|
| 0 | u32 | Time of the optical readings, Unix seconds |
| 4 | u8 | Heart rate, 0 for none |
| 5 | u8 | HRV, 0 for none |
| 6 | u8 | SpO2, 0 for none |
| 7 | u8 | Record type: 5 breathing, 6 exercise, 100 and above not worn |
| 8 | u32 | Time of the temperature readings |
| 12 | f32 | Highest skin temperature, °C |
| 16 | f32 | Lowest skin temperature, °C |
| 20 | u32 | Time of the activity readings |
| 24 | u8 | Movement intensity (Gadgetbridge scales by 1/150) |
| 26 | u16 | Steps |
| 28 | u8 | Stress, 0 to 255 (Gadgetbridge scales to 0 to 100) |
| 30 | u16 | Record number |

Bytes 25 and 29 are not decoded. Records are not deleted on read, so syncing again is safe, and a
sync can resume from the last record number stored.

## State (`86f61001`)

7 bytes: battery percent in byte 0, state in byte 5 (`00` normal, `03` charging), ring temperature
in byte 6. Bytes 1 to 4 are not decoded.

## Live data (`86f66001`)

- Exercise, 11 bytes: op `62`, then time (u32 at byte 3) and heart rate (byte 7).
- Breathing, 27 bytes: op `72`, time (u32 at 3), heart rate (byte 7), HRV (byte 9), temperature
  (f32 at 11).

## Not covered by Gadgetbridge

Sleep and sleep stages (the vendor computes them in its cloud), a battery request (state
notifications give it), firmware updates. Ringmaster's own sleep estimate would have to work from
heart rate, HRV, intensity and temperature, as it does for RingConn.

## Fit with Ringmaster

Close to Colmi's path: no authentication, history as values that go into `samples` and `steps`, a
resume point (the last record number) kept per ring. Only the sleep estimate needs new work. A
connection from Ringmaster sets the time, which matters if the vendor app is also used.
