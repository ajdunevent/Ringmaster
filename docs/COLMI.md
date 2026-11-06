# Colmi rings

Protocol notes for the Colmi R02 family (R02 to R12, and rebadged rings sold as Yawell and others),
as implemented in `crates/colmi`. Nothing here has been checked against a ring yet. Each fact
gives its source.

A ring sold as an "R12" can be a different family altogether: the R12 tested in 2026-11 is an
RWfit ring (RWFit app, `docs/RWFIT.md`) and does not speak this protocol. The app a ring's box
names is the quickest check: Colmi rings point to QRing or Da Rings.

Sources:

- **GB**: Gadgetbridge's Colmi R0x driver (codeberg.org/Freeyourgadget/Gadgetbridge, `yawell/ring`,
  checked against its source on 2026-10-27). AGPL, so facts only were taken from it; no code.
- **T**: tahnok/colmi_r02_client (MIT) and its notes, including captured packets used as test
  fixtures here.

## Finding the ring

- Advertised names start with the model and an underscore, such as `R02_A1B2` (GB, T). Ringmaster
  also accepts `COLMI`, `Y27_` and `H59_` prefixes, and either service below.
- No bonding and no authentication (GB, T). Ringmaster connects without pairing.

## Services

| Service | Write | Notify | Use |
|---|---|---|---|
| `6e40fff0-b5a3-f393-e0a9-e50e24dcca9e` | `6e400002-…` | `6e400003-…` | Commands, 16-byte packets |
| `de5bf728-d711-4e47-af26-65e3012a5dc7` | `de5bf72a-…` | `de5bf729-…` | "Big data": SpO2, sleep, temperature (newer firmware) |

Older models may lack the second service; Ringmaster then skips SpO2, sleep and temperature
history.

## Command packets

16 bytes: command, up to 14 payload bytes, checksum. The checksum is the sum of the first 15 bytes
modulo 256 (T). Replies start with the same command byte.

| Command | Request payload | Reply |
|---|---|---|
| `01` set time | year % 100, month, day, hour, minute, second, all BCD, local time | |
| `03` battery | none | `[03, percent, charging]` |
| `04` phone name | `02 0a` then two letters | |
| `15` heart rate history | the day's local midnight as a little-endian u32, written as if it were UTC (T) | see below |
| `16` heart rate interval | `02 01 minutes` on, `02 02 00` off (GB) | |
| `2c` SpO2 auto, `36` stress auto, `38` HRV auto | `02 on` (GB) | |
| `3a` temperature auto | `03 02 on` (GB) | |
| `37` stress history | none; today only (GB) | half-hourly, see below |
| `39` HRV history | days ago as a little-endian u32 (GB) | half-hourly, see below |
| `43` activity history | `days_ago 0f 00 5f 01` (T) | see below |
| `69` live reading | `kind action`: kind 1 heart rate, 3 SpO2; action 1 start, 3 continue (T, GB) | `[69, kind, error, value]` |
| `6a` stop live reading | `kind 00 00` (T) | |
| `1e` real-time heart rate | `01` start, `02` stop, `03` continue (GB) | `[1e, bpm]` once a second |
| `73` notification | sent by the ring | `[73, 0c, percent, charging]` is a battery change (GB) |

### Activity (`43`)

A header `[43, f0, …]` whose byte 3 is 1 when calories are counted in tenths, then one packet per
quarter hour with data: BCD year, month and day in bytes 1 to 3, the quarter (0 to 95) in byte 4,
this packet's number and the total in bytes 5 and 6, then little-endian calories (7, 8), steps
(9, 10) and distance in metres (11, 12). `[43, ff]` means no data for that day. (T, GB)

### Heart rate (`15`)

Packet 0 carries the packet count in byte 2 and the measuring interval in minutes in byte 3.
Packet 1 echoes the timestamp in bytes 2 to 5 and holds the first 9 readings from byte 6; later
packets hold 13 readings from byte 2. Reading *n* is at minute *n* × interval. Zero means no
reading. Today's answer has slots for the rest of the day, which Ringmaster drops. (T) For today,
Gadgetbridge sends the current time rather than midnight; both fall in the same day.

### HRV (`39`) and stress (`37`)

Packet 0 carries the count in byte 2. Packet 1 holds 12 values from byte 3, later packets 13 from
byte 2, one value per half hour; four data packets cover the day. (GB)

## Big data

Requests and answers on the second service: `bc type length(2, LE) crc(2, LE) payload`. The CRC is
CRC-16/MODBUS of the payload. Answers longer than one notification continue in the next ones.

| Type | Request | Payload |
|---|---|---|
| `2a` SpO2 | `bc 2a 01 00 ff 00 ff` | per day, oldest first: days ago, then 24 × (lowest, highest) for each hour |
| `25` temperature | `bc 25 01 00 3e 81 02` | per day: days ago, a byte seen as `1e`, then 24 × (value at :00, value at :30); °C = value / 10 + 20 |
| `27` sleep | `bc 27 01 00 ff 00 ff` | session count, then per session: days ago, length, start and end (u16 LE, minutes of the day), then (stage, minutes) pairs |

Sleep stages: 2 light, 3 deep, 4 REM, 5 awake. A start later than the end means the night began the
evening before. (GB)

## How Ringmaster uses it

- On connect: set the clock, send the phone name, turn on heart rate every 10 minutes and SpO2, HRV,
  stress and temperature, then ask for the battery.
- Sync: 7 days of activity, heart rate and HRV, today's stress, then SpO2, temperature and sleep.
  The rings keep their history after it is read, so a sync can repeat safely; newer values replace
  stored ones.
- Storage: heart rate, HRV, SpO2 (the middle of each hour's range, plus the lowest as `spo2_low`),
  skin temperature and stress go in `samples`; steps in `steps`; the ring's own sleep and stages in
  `ring_sleep` and `sleep_stages`.
- Quick reading: `69 kind 01`, a continue every 8 s, `6a` at the end. The continue cadence is a
  guess to check on the ring. Gadgetbridge uses `69 01` for a single heart-rate reading, and `1e`
  for streaming: the ring stops after 60 s unless a `1e 03` arrives, which Gadgetbridge sends every
  30 readings. If `69` turns out to give one value, heart rate should move to `1e`.
- With no traffic for 180 s the link check asks for the battery; the answer also keeps the battery
  shown current.

## To check on the first ring

- Name prefix and services of the delivered model, and whether it has the big-data service.
- Whether the automatic settings stick, and their battery cost.
- Whether the temperature answer is skin temperature, and its offset against RingConn's.
- Live reading length without continues, and whether continues extend it.
- Whether stress and HRV values line up with the half-hour slots as decoded.
