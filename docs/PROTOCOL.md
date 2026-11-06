# RingConn BLE Protocol

Ringmaster's working spec for the RingConn Gen 2 (and, where noted, Gen 3). Everything here is
observed behaviour from independent reverse engineering, not vendor documentation. Facts are
stated in our own words and implemented from scratch; no third-party code or text is copied.

Compiled 2026-10-21. **RM** tags come from the first ringprobe run on 2026-10-21: Gen 2,
FW FR02.018, hardware `00010002`, on an FLX1s (FuriOS forky, BlueZ 5.85), history drained.
Section 13 tracks what is still open.

## Conventions

**Confidence**

- **Confirmed**: reproduced by at least one source on real hardware, or by us.
- **Probable**: consistent with captures but resting on limited evidence.
- **Unverified**: inferred, disputed, or seen once.

**Sources**

| Tag | Source | Notes |
|---|---|---|
| OC | [OpenCircuit](https://github.com/perezjuanj/OpenCircuit) `docs/PROTOCOL.md` | Primary RE effort. Gen 2 FW FR02.018 and Gen 3 FW FR05.011. Now PolyForm Noncommercial; an earlier MIT snapshot exists at [7sumCreations/OpenCircuit](https://github.com/7sumCreations/OpenCircuit). Facts only; do not copy code or prose. |
| RL | [ringlink](https://github.com/zazaulola/ringlink) `docs/PROTOCOL.md` and source | Working Android client, Gen 3 FW FR05.010 to FR05.012. MIT. |
| GB | [Gadgetbridge PR #6508](https://codeberg.org/Freeyourgadget/Gadgetbridge/pulls/6508) | Draft Gen 2 driver, opened 2026-08-02. AGPL. |
| RM | Ringmaster | Checked by us. |

**Notation**: bytes in hex, multi-byte integers big-endian unless stated, offsets zero-based from
the first byte of the frame (`[4:6]` means bytes 4 and 5).

## 1. Safety

- **Never probe unknown opcodes.** A blind sweep of opcode `0x21` bricked a ring during RE; it
  needed a charger restart, forgetting the device, and re-pairing. New capabilities come from
  capturing the official app and replaying exactly what it sends. (RL, confirmed)
- **History ACKs are destructive.** Acknowledging a page lets the ring discard it (section 7).
  Persist every page durably before its ACK is written. (OC, RL, confirmed)

## 2. GATT layout

**Primary data service** `8327ad99-2d87-4a22-a8ce-6dd7971c0437` (OC, RL, confirmed)

| Role | Characteristic | Properties |
|---|---|---|
| Commands (host to ring) | `8327ad98-2d87-4a22-a8ce-6dd7971c0437` | write with response |
| Everything back (ring to host) | `8327ad97-2d87-4a22-a8ce-6dd7971c0437` | notify; enable via CCCD `01 00` |

All commands, replies, history pages and telemetry travel over this one pair. There are no
per-metric characteristics.

**Secondary service** `1d14d6ee-fd63-4fa1-bfa4-8f47b42119f0` with write characteristics
`f7bf3564-fb6d-4e53-88a4-5e37e0326063` and `984227f3-34fc-4045-a5d0-2c581f81a153`. Never used by
the official app in any capture; probably OTA. Leave alone. (OC, unverified role)

**Device Information Service** `0x180a` (OC, RL, confirmed). Example values from OC's Gen 2:

| Characteristic | Example |
|---|---|
| Manufacturer `0x2a29` | `JZ_Tech` |
| Model `0x2a24` | (not recorded) |
| Serial `0x2a25` | `RCA1F252311002B09` |
| Firmware `0x2a26` | `FR02.018` (Gen 2 is `FR02.*`, Gen 3 is `FR05.*`) |
| Hardware `0x2a27` | `00010001` |
| System ID `0x2a23` | the ring's MAC |

DIS needs no vendor command and is the reliable way to tell generations apart; the advertised
name (`RingConn Gen2-XXXX`, suffix = last two MAC bytes) can be changed by the user. (RL)

**RM, Gen 2 FR02.018:** layout matches the tables above, including the secondary service. DIS has
no model (`0x2a24`) or software (`0x2a28`) characteristic; System ID `0x2a23` is the 6-byte MAC in
display order. The advertisement lists `0x1800`, `0x1801`, `0x180a`, `1d14d6ee` and `8327ad99`,
with a public address.

## 3. Link security

**An LE bond is required.** An unbonded central can connect, subscribe and complete the `01`
status exchange, but the ring silently drops every data command. (OC, RL, confirmed)

The bond is LE Secure Connections "Just Works" (ring IO capability NoInputNoOutput) with keys
generated locally by ECDH. No cloud key, no account, no vendor app. A factory-fresh Gen 3 that
had never seen the official app worked with an independent client. (OC, confirmed)

Bonds belong to the phone's Bluetooth stack, not to an app, so a bond made by any app is reusable.
The ring supports several bonded phones. (OC, RL, confirmed)

GB's PR says no pairing secret is needed, which is consistent with Just Works, but does not
mention the bond explicitly.

**RM, Gen 2 FR02.018, BlueZ 5.85 on FLX1s (2026-10-21):** host-initiated `Pair()` creates the bond
without any agent prompt. On the FLX1s the `Pair()` call itself may not return even after the bond
exists (in 3 of 3 successful runs it was still pending after 40 s), so the bond state has to be
polled. Directly afterwards, reading GATT services fails with "GATT services have not been resolved"
while BlueZ re-reads them. With the ring in its closed charging case, pairing was cancelled
("Authentication Canceled") and enabling notifications timed out. There is no separate pairing
mode: RingConn's instructions are to remove the old phone-side pairing, wake the ring in the case,
and search; a restart is 5 quick insertions into the case within 10 s (blue light).

**RM, BlueZ 5.87 on Fedora 44 x86_64 in Distrobox (2026-10-21 16:11):** `Pair()` returned success
after 1.1 s and services were readable 1.0 s later, so the hanging `Pair()` is specific to the FLX1s.
Distrobox does not link the host's system bus into the container; BlueZ works through
`/run/host/run/dbus/system_bus_socket`.

## 4. Framing

The two directions are not symmetric. (OC, RL, confirmed)

**Commands** (host to ring): `[cmd] [sub] [payload...] 00`

- Sent verbatim, no checksum. The final `00` is a literal byte.
- Appending an XOR trailer produces a frame the ring ignores. The ring ATT-acks every write but
  only acts on valid contents, so a malformed command fails silently.

**Responses** (ring to host): `[cmd ^ 0x80] [payload...] [xor]`

- The trailing byte is the XOR of all preceding bytes.
- **Exception:** `0x50` end-of-history frames carry no XOR trailer (section 7.6). A strict
  validator would reject exactly the frame that signals completion.
- Some ring-initiated frames do not follow the `^ 0x80` rule (`0x10`, `0x11`); see section 9.

## 5. Session setup

Run on every connection, after subscribing to notifications:

```
host  01 00 00
ring  81 00 <challenge> <xor>
host  01 01 <r0> <r1> <r2> 00
ring  81 01 ...                     (38 bytes; contents mostly undecoded)
```

**Authentication** (OC, RL, GB, confirmed; algorithm RE'd from the official app by OC, verified
on 24 captured pairs by OC and on Gen 3 by RL):

```
V = mac[3] ^ mac[4] ^ mac[5]                   mac in display order, F8:79:99:F7:03:AD -> F8 79 99 F7 03 AD
r = SM3(bytes([V, challenge]))[29:32]          last three bytes of the 32-byte digest
```

SM3 is the Chinese national hash, GB/T 32905. The only key material is the ring's MAC.

**Test vectors** (RM, confirmed with an independent SM3 implementation):

| MAC | V | challenge | r |
|---|---|---|---|
| `F8:79:99:F7:03:AD` | `59` | `e5` | `52 0b e1` |
| `F8:79:99:F7:03:AD` | `59` | `b0` | `31 82 67` |
| `F8:79:99:F8:55:BB` | `16` | `59` | `2e e9 dc` (accepted by a Gen 2 ring, RM) |

Older OC notes describe the `01 01` argument as a per-session nonce; it is this auth reply.

**RM:** authentication succeeds on Gen 2 FR02.018 using the MAC exactly as BlueZ reports it. The
`81 01` reply (38 bytes) begins `81 01 00` followed by bytes matching descriptor bytes `[1:13]`
(battery, state, steps, temperatures); the rest is undecoded. (Probable)

**Waking telemetry.** After auth, send `07 00 00`; it answers with a `0x87` descriptor within
0.1 s. `d0 00 00` drew no answer within 6 s. (RM, confirmed on Gen 2 FR02.018; RL found the same
on Gen 3.) Neither request is destructive.

## 6. Command reference

| Purpose | Command | Reply | Notes | Conf. |
|---|---|---|---|---|
| Status / challenge | `01 00 00` | `81 00 <chal> <xor>` | Only command answered before auth | confirmed |
| Auth reply | `01 01 r0 r1 r2 00` | `81 01 ...` | Section 5 | confirmed |
| Sync open | `02 00 <cursor:4> <channel> 01 00` | `82 ...` | Section 7 | confirmed |
| Fetch | `07 00 00` | `87`, or pages | Starts a stream; also returns a descriptor | confirmed |
| Status query | `d0 00 00` | `10` | Required before live modes | probable |
| Live HR mode | `06 01 00` | `86 00 86` | Section 8 | confirmed |
| Live SpO2 mode | `06 02 00` | `86 00 86` | Section 8 | probable |
| Leave mode | `06 00 00` | not recorded | Ends sport mode; for live modes RL infers it and sends it best effort | probable |
| Sport start | `06 03 07 04 00` | `86 00 86` | Only from idle; meaning of `07 04` unknown | probable |
| Live poll | `95 00 00` | `15 ...` | No faster than every 2 s | confirmed |
| ACK `0x47` page | `c7 00 00` | next page | | confirmed |
| ACK `0x4c` page | `cc 00 00` | next page | | confirmed |
| ACK `0x4d` page | `cd 00 00` | next page | | probable |
| ACK `0x4e` frame | `ce 00 00` | | Live sport frames | probable |
| Heartbeat ACK | `91 00 00` | | Constant; does not echo | confirmed |
| Find-my-ring LED | `24 01 00` / `24 00 00` | | Lights the ring blue | confirmed (Gen 3) |
| Vibrate | `0b 03 01 64 00` | `8b 00 8b` | Gen 3 only; Gen 2 has no motor | confirmed (Gen 3) |

Sources: OC for all rows; RL additionally confirms the LED, vibrate, `0x4d`/`0x4e` ACKs and
leave-mode. OC also documents a 100 Hz raw-PPG mode (`06 05 00`) and a sensor readout
(`29 00 00`), both out of scope for now. The official app (3.10.0, decompiled) starts its apnea
PPG stream with mode `04` in the same command, `06 04`, and decodes the resulting frames `12`
(section 14). Unverified on the tested ring.

## 7. History sync

### 7.1 Channels

The sync-open `channel` byte selects one of several independent histories, each with its own
resume pointer on the ring. Draining one does not disturb another. (OC, RL, confirmed)

| Channel | Name | Carries |
|---|---|---|
| `00` | "sleep" | `0x4c` epochs and `0x47` pages |
| `03` | all-day | `0x4c` epochs (incl. daytime SpO2 roughly every 10 min) and `0x47` pages |
| `02` | sport | `0x4d` 10-second records (RL; Gen 3 observed, Gen 2 unknown) |

Despite the names, records are not cleanly split by time of day: RL found channel `00` streams
continuously around the clock, and OC saw a Gen 3 deliver a night's epochs on channel `03`. Merge
all channels and deduplicate on the record counter. Nothing in the protocol marks sleep.

**RM, Gen 2 FR02.018:** channel `00` delivered 7 continuous days, day and night (3999 epochs,
680 optical records, 95 s). Channel `03`, opened immediately afterwards, delivered no pages.

**History depth (RM, probable):** that first sync came more than four months after the official
app last synced, yet it held exactly 7.03 days of epochs and 7.08 days of `0x47` records. The ring
keeps about 7 days; anything older is overwritten. The official app's own history has gaps of 11.8
and 7.8 days, consistent with this, though not wearing the ring would look the same.

**Rebuilding epochs from the official app (RM, confirmed on 3,000 sampled records):** the app
keeps the ring's own bytes beside its cleaned values. `[4]` is `rawHr` (the app's `pr` zeroes the
sentinels 4 and 5), `[5]` `rawHrv`, `[6]` `confidence`, `[7]` `resprate`, `[8]` `rawSpo2` (SpO2, or
the activity tag `11` to `13`; the app's `spo2` column is often 1 to 3 points higher), `[9]` `item2p5`,
`[10:15]` `envLights`, `[15:23]` `actiCountsString` packed as five 12-bit big-endian values; the
4-bit flag is not stored. Ringmaster stores the counter as `utcTs` - 1577793600; nights land at
plausible local bedtimes that way.

**Official sync schedule:** the official Android app runs a background job every 15 minutes
(WorkManager, interval 900 s, flex 300 s; 5,653 runs by 2026-09-12).

### 7.2 Cursor

`02 00 <cursor:4> <channel> 01 00`. The cursor is seconds since an epoch (section 7.5).

The cursor is a trigger, not a bound. Opening at roughly "now" makes the ring stream everything
not yet delivered on that channel, up to its current time, and records may overshoot the cursor by
minutes while the drain runs. The ring tracks its own resume pointer; the host persists nothing.
(OC, confirmed)

`FF FF FF FF` (far future) does not pull history. The official app never sends it. (OC, probable)

### 7.3 Sync-open reply

`82 [b1] [b2] [xor]`. Observed: `82 00 00 82` when pages follow; `82 ff 00 7d` when the channel's
pointer is already at the end (nothing to drain), which lets the client skip waiting.
(OC, RL, probable)

**RM:** `82 00 00 82` is not a promise of pages. Gen 2 channel `03` replied `82 00 00 82` and then
sent nothing but `0x87`. The drain loop has to detect an empty channel by itself (section 7.4).

### 7.4 Drain loop

```
for channel in (00, 03, 02):
    write 02 00 <cursor> <channel> 01 00
    if reply b1 == ff: next channel
    write 07 00 00
    loop on incoming frames:
        0x4c page     persist, then cc 00 00
        0x4d page     persist, then cd 00 00
        0x47 page     (persist or discard), then c7 00 00
        0x11          91 00 00
        0x87 / 0x10   07 00 00          a new header mid-stream
        0x50          end of this channel
        silence       see below
```

(RL, confirmed on Gen 3; OC equivalent)

**The fetch/`0x87` exchange (RM, confirmed on Gen 2).** The ring answers each page ACK with a
`0x87` descriptor; the next page comes in reply to the following `07 00 00`. The first fetch after
a sync open also draws only `0x87`. Once a channel has nothing left, **every** fetch draws another
`0x87`, so "on `0x87`, send `07 00 00`" loops forever: in the first probe it ran at about 22 writes
per second for 16 minutes, until the ring rejected writes with ATT error `0x0e`. A fetch may be
re-sent on `0x87` only when a page has arrived since the previous fetch; a few consecutive fetches
answered by `0x87` alone mean the channel is empty.

**Ending without `0x50`.** On some Gen 3 firmware a channel goes quiet mid-backlog with no `0x50`.
OC's response is to send `07 00 00` again at the quiet point and reopen the channel while each
round still yields new records. (OC, probable)

**Page header.** `[0]` opcode, then **`[1:3]` a 16-bit remaining-record count** (`00 00` on the
last page), then fixed-size records, then the XOR byte. Records never span pages. GB's reading is
right: RM saw `4c 0f bc` (4028 remaining, falling by 6 per page) and `47 02 a3` (675, falling by
5). OC and RL describe `[1]` as always `00`, which holds only for backlogs under 256 records.
(RM, confirmed)

**Overlap.** The range reported by `0x50` can trail the last record delivered, so consecutive syncs
re-deliver a few records. Deduplicate on the counter. (OC, confirmed)

### 7.5 Epoch: disputed

Two constants circulate, exactly 4 hours apart:

| Constant | UTC | Used by |
|---|---|---|
| `1577793600` | 2019-12-31 12:00:00 | OC for both cursor and records; RL for records |
| `1577808000` | 2019-12-31 16:00:00 | GB; RL for the cursor the official app writes |

RL's evidence (Gen 3): with `1577808000`, the newest record of a sync lands four hours after the
sync that fetched it, which is impossible; with `1577793600` it lands minutes before. Yet the
official app's sync-open cursor matches capture time only under `1577808000`. OC instead matched
20 Gen 2 sync-opens to wall clock under `1577793600` within 0.5 s.

RL also measured an unexplained 4-hour discontinuity in one ring's counters during continuous wear.

**RM, Gen 2 FR02.018:** with `1577793600`, the newest record of a drain lands 1.6 min before the
sync that fetched it; with `1577808000` it lands 238 min after, which is impossible. The sync was
opened with a `1577793600` cursor and drained normally.

**Policy:** store raw counters and date records with `1577793600` (confirmed for Gen 2 records).
Use the same constant for the cursor. The 4-hour discontinuity RL reports remains unexplained.

Record counters are full 32-bit values; the leading byte is `0c` today but becomes `0d` in late
2026. Do not treat it as a delimiter. (RL, confirmed)

### 7.6 `0x50` end-of-history

`50 00 [n] [entries...]`, no XOR trailer. Entries are 6 bytes: `[type] [value] [cursor:4]`.
Arrives after the last page of a channel. OC reads the entries as an event log of at most 40,
oldest first, with `[2]` counting entries not shown; type `10` with values `0f` then `0a` appears
to bracket the ring's own detection of an activity session. Only the end-of-channel role matters
for a first version. (OC: layout confirmed, meanings probable to unverified)

**RM:** the `0x50` that ended channel `00` and the one that later arrived on channel `03` were
byte-identical (`50 00 6a ...`), which supports OC's event-log reading over a per-channel range.

## 8. Live measurement

**Heart rate** (OC, RL, confirmed):

```
d0 00 00                      required; without it the ring stays in bulk mode
06 01 00   -> 86 00 86
07 00 00
wait ~10 s for warm-up
95 00 00   -> 15 00 <hr> ... <xor>      repeat every ~2 s
06 00 00                      on exit, best effort
```

The first samples are a warm-up sentinel (around 8); treat values below 30 as "not locked".
**Poll no faster than every 2 s.** Faster polling keeps resetting the ring's averaging window and
the reading never leaves the sentinel. Requires the ring to be worn with good contact.

**RM, Gen 2 FR02.018 (2026-10-21 13:59, worn, warm hand):** four start-up sequences were tried for
40 s each, and all four gave real readings:

| Sequence | Heart rate readings |
|---|---|
| `d0`, 0.4 s, `06 01 00`, 0.4 s, poll `95` every 2 s (ringlink) | 8, then 60 to 61 |
| `d0`, `06 01 00`, `07`, poll every 2 s from 10 s (OpenCircuit) | 8, then 60 to 65 |
| `d0`, `06 01 00`, `07`, no polls | 8, then 59 to 62 |
| `d0`, 0.4 s, `06 02 00`, 0.4 s, poll every 2 s | SpO2 97 to 99 |

- The ring **streams** `0x15` every ~2 s without being polled; polls neither add frames nor shift
  their timing. (ringlink states that its Gen 3 must be polled.)
- A session is fixed-length: the first frame arrives ~10 s after the mode command and is always the
  warm-up value `08`; about 10 real readings follow at 2 s intervals; then the ring stops on its own
  (~30 s after the mode command) and sends a descriptor with state `02`. `06 00 00` answers
  `86 00 86`.
- The morning's readings stuck at `08` came with skin temperature at 25 to 27 degC on a cold
  morning; the afternoon run read 27.5 to 31.8 degC. Poor perfusion is the likely cause.
- `d0 00 00` drew a `0x10` in one session (after ~3 s) and nothing in another.

**SpO2 frame (RM, confirmed):** `15 01 00 00 <a:4> 00 00 <b:4> <spo2> 00 <xor>`, 17 bytes; byte
`[14]` is SpO2 % (`58` to `64` = 88 to 100 seen). Values below 70 (`02`, `03`, `05`, `08`, `0a`)
mean no reading yet; the first ones in a session come with `a` and `b` zero. Frames can arrive in
bursts (three within 120 ms after a 6 s gap).

`a` and `b` are 32-bit little-endian, each 1.4 to 92 million. Neither tracks SpO2 alone (r 0.07 and
-0.02), but ln(a/b) does (r 0.85 over 1,194 readings, 0.88 per-session means): the red/infrared
ratio a pulse oximeter computes SpO2 from, here SpO2 = 92.6 + 15.9 ln(a/b) with 0.66 points of
scatter. (RM, 2026-10-22 night, probable)

**Back-to-back SpO2 sessions overnight (RM, 2026-10-22, Gen 2 FR02.018, `ringprobe --overnight`):**
each session gives 10 frames 2 s apart, the first valid reading 11 s after the mode command, and
ends by itself after 30 to 31 s with a state `02` status frame; a new session can start 0.5 s later.
While worn this gave one reading every 3.2 s for an hour (1,171 readings). Battery held at 91 %
(4260 to 4249 mV) over 1.4 hours of sessions. A ring that is off the finger answers the mode
command, then sends a state `01` (settling) status frame and no samples. After a link drop, BlueZ
can reconnect a trusted ring by itself; writes then succeed, but notifications stay off and the
ring is not authenticated, so nothing comes back until the link is rebuilt.

**Live sessions stop the ring's own recording (RM, confirmed):** the next sync had no `0x4c`
epochs from 23:32 to 01:02, exactly the span of back-to-back sessions, including the hour the ring
was worn and gave live SpO2. Recording resumed when the sessions stopped. Whether spaced-out
sessions leave epochs in between is untested; `ringprobe --overnight` rotates pause lengths to find
out.

**RM, worn with skin at 34 degC (16:11 run):** all three heart-rate sequences gave a real reading
from the second frame (68 to 74 bpm).

**SpO2**: as above with `06 02 00`; replies are long `15 01 ...` frames with SpO2 % at `[14]`.
`[2]` is `00` in this mode, so do not read heart rate from it. (OC, probable)

The official app drains history before entering live mode. Whether live mode works without a
prior sync open is untested; OC's own live path opens with `FF FF FF FF` to skip the backlog.
(Unverified)

## 9. Ring-initiated frames

### 9.1 Descriptor `0x10` / `0x87` (19 bytes)

Arrives in reply to `d0 00 00` (`0x10`) or `07 00 00` (`0x87`), and on its own every 30 s to
2 min while a session is open. Same layout either way; XOR-valid. (OC, RL, GB, confirmed)

| Offset | Meaning | Conf. |
|---|---|---|
| `[1]` | Battery % | confirmed |
| `[2]` | State: `01` settling, `02`/`03` idle (alternates), `04` on charger, `05` after work mode 2 (section 14), `06` sport mode active | confirmed |
| `[4:6]` | Steps in the current quarter-hour bucket, **not** a daily total; zero is normal | confirmed (OC); GB calls it an accumulator |
| `[6:8]` | Skin temperature channel 1, 0.1 degC | confirmed |
| `[8:10]` | Skin temperature channel 2, 0.1 degC | confirmed |
| `[14:16]` | Battery voltage, mV | confirmed |
| `[17]` | Charging case: low 7 bits case %, bit `0x80` case plugged in, `ff` ring not in case | confirmed |

**RM, Gen 2 FR02.018:** offsets confirmed. Example `87 49 02 00 00 39 01 07 01 0a 00 00 00 00 0f fa
5b ff 3e` decodes as 73 %, idle, 57 steps, 26.3 / 26.6 degC, 4090 mV, not in case. OC's "`01`
prefix" on the temperatures is the high byte of the 16-bit value (`01 07` = 263). The step field
dropped from 57 to 0 during the run, consistent with a quarter-hour bucket. `[16]` varied between
`58` and `5b`; meaning unknown. Unprompted `0x10` frames arrived at alternating gaps of about 42 s
and 108 s, matching OC's 40/110 s timer. Skin temperature also arrives in history as the
`0x47` record (section 10.2), one sample a minute per sensor. The ring keeps streaming on the charger. GB reports
`0x10`/`0x87` arriving about every 14 s on Gen 2.

### 9.2 Heartbeat `0x11`

`11 00 <counter> <token> <xor>` roughly every 2.5 minutes when idle; the counter resets per
connection. Reply `91 00 00`. (OC, RL, confirmed)

### 9.3 Live sport `0x4e`

HR and steps about every 10 s while sport mode is active. ACK `ce 00 00`. Layout not yet recorded
here. (OC, RL, probable)

**While the ring believes an activity session is open, it stops unsolicited pushes** but keeps
the link up and keeps recording. (OC, confirmed on Gen 3)

## 10. Record formats

### 10.1 `0x4c` epoch, 23 bytes, one per 2.5 minutes

Counter step `0x96` = 150 s. (OC, RL, GB, confirmed)

| Offset | Meaning | Conf. |
|---|---|---|
| `[0:4]` | Counter, u32 | confirmed |
| `[4]` | Heart rate, bpm; below 30 means unmeasured | confirmed |
| `[5]` | HRV (RMSSD), ms | confirmed |
| `[6]` | Signal confidence, 0 to about 12 | confirmed |
| `[7]` | Respiratory rate x 8 | confirmed |
| `[8]` | SpO2 %, **or** a layout tag `11`/`12`/`13` on activity epochs | confirmed |
| `[9]` | Activity class: tens digit rises with movement intensity (1 to 7), units digit 0 to 5 counts restlessness; `0a` at rest | probable |
| `[10:15]` | Five 30-second motion counts | confirmed role |
| `[15:23]` | Five 12-bit magnitudes, nibble-packed, then a 4-bit flag (low nibble of `[22]`) | confirmed layout, unknown meaning |

**Layouts**, discriminated structurally (RL):

- **Idle / unworn**: `[4:8] = 05 00 0c 00`, `[9] = 0a`, `[10:15]` all `01`, `[15:22]` all `00`.
- **Activity**: `[8]` is `11`, `12` or `13`. `[4]` is still heart rate here; RL showed values run
  continuously across alternating epochs, ruling out OC's older step-count reading.
- **Sleep vitals**: anything else; `[8]` is SpO2.

Valid SpO2 is 70 to 100 (GB). Do not treat a real low reading as a sentinel; use the layout.

**RM, Gen 2 FR02.018, 7 days (3999 epochs):**

- The "activity" layout is not exercise. It made up 2982 of 3999 epochs, nights included; it marks
  an epoch without a SpO2 reading. Through the night the layouts alternate epoch by epoch (SpO2 every
  5 minutes); by day a SpO2 epoch comes roughly every fourth epoch.
- `[15:23]` is not skin temperature. The five values swing between 0 and about 2100 and fall to
  zero overnight, so they track motion in some form.
- Overnight the five motion counts sit at `01` and confidence `[6]` stays at 9 to 10. A still
  evening also shows `01` motion, but with confidence mostly 1 to 5. Ringmaster's sleep estimate
  uses that difference (section 11).

### 10.2 `0x47` skin temperature, 47 bytes, one per 15 minutes

Counter step `0x384` = 900 s. `[9:47]` holds thirty 10-bit big-endian samples in units of
0.05 degC: fifteen pairs, one pair a minute, alternating between the two skin sensors. The record's
counter marks the last pair: on 2026-10-14 the cooling and rewarming line up with the epochs'
heart rate only that way. `[4:9]` is not a temperature; its meaning is unknown. Off the finger
the ring records no `0x4c` epochs while these samples fall to room temperature, so Ringmaster keeps
a sample only within 150 s of an epoch with a heart rate (2.3 % of a week's samples dropped). OC and RL called this an optical trend and discarded
it. (layout OC, RL; meaning RM, probable)

**RM, Gen 2 FR02.018, 680 records:** overnight medians 32.5 to 32.8 degC, the two sensors within
about 0.1 degC. On 2026-10-14 at 21:37 the samples fall from 32.4 to 25.0 degC along a cooling
curve while no heart rate is read, hold at room temperature, and climb back to 32 degC within
about 10 minutes after 22:15. The vendor app stores the matching history in a table with four
temperature columns (`HistoryActivitySyncInfo`) and uploads it.

### 10.3 `0x4d` sport interval, 11 bytes, one per 10 seconds

`[0:4]` counter, `[4]` heart rate (valid 30 to 220), `[5]` steps. Remaining bytes undecoded.
Delivered on channel `02`, including gaps when the phone was out of range during a sport session.
(RL, probable; Gen 2 unverified)

## 11. Not on the wire

Computed by the vendor app, not transmitted: sleep stages, sleep score, stress, readiness,
recovery, blood pressure and vascular trends, nightly respiratory summaries. Daily step totals are
also not transmitted as such. Anything Ringmaster shows for these is its own estimate and should be
labelled that way. (OC, RL, confirmed)

**RM sleep estimate.** An epoch counts as sleep-like when all five motion counts are at `01`, a
heart rate was read and confidence is at least 8. An epoch is asleep when at least 75 % of the
epochs within 15 minutes either side are sleep-like; runs are extended over adjacent sleep-like
epochs, joined across wakings shorter than 45 minutes, broken by data gaps over 10 minutes, and
kept from 45 minutes up. Labelled as an estimate in the app.

**Checked against the official app (RM, 2026-10-22).** The app's private database from the
user's old phone (`ring_conn.db`, unencrypted SQLite) holds 87,237 epochs from 2025-12-04 to
2026-06-03 and its own result for 158 nights. Run on those epochs, the rule above found all 158
nights; median edges 10 minutes before the app's sleep start and 8 minutes after its sleep end;
91 % of the estimated time fell inside the app's sleep or naps. The app separates time in bed from
sleep (median 17.5 minutes to fall asleep, 9.4 minutes from waking to getting up), and the
estimate tracks time in bed more closely than sleep. A confidence threshold of 9 matched the app
better (95 %) but split nights recorded on the same ring in 2026-10.

The app stores each epoch with the decoded fields named: `pr` heart rate, `hrv`, `resprate`
(breaths per minute times 8), `confidence`, `item2p5` (byte `[9]`, usually `0a`), the five motion
counts as a JSON list in a column named `envLights`, and five further counts per epoch in
`actiCountsString` (values up to about 2,500; zero while still). Blood oxygen is a separate table
on the same timestamps, with 0 where no reading was taken.

**Sleep stages in the official app (RM, 2026-10-22).** `sleepPhases` codes: 1 deep, 2 light, 3 REM,
4 awake, on the same 2.5-minute grid as the epochs. Across 158 nights:

- Deep and REM never follow each other directly, and waking leads to light sleep (921 of 926).
- Deep is placed by time of night: 47 to 61 % of the first two hours, 5 % in the fourth, none
  after about five hours. REM rises from 3 % in the first hour to about 25 % from the fourth.
- Byte `[9]` above 10 marks 64 to 100 % of the epochs scored awake inside a night; it does not
  separate deep, light and REM.
- With heart rate and HRV taken relative to the night's own mean, REM has higher heart rate and
  lower HRV than light sleep. Deep shows a higher heart rate than light only because it falls
  early, when heart rate is still falling.
- Inside the app's sleep period, a model of time of night, heart rate, HRV, respiratory rate,
  motion and byte `[9]` matched 81 % of epochs on nights it was not fitted to (kappa 0.68):
  deep 86 %, light 86 %, REM 67 %, awake 58 %. Time of night alone gave 69 %.
- Staging runs on the vendor's server: the app's request log shows `HrSpo2Upload` going up and
  `SleepSync` coming back, with no sleep upload. The server's sleep record also carries a skin
  temperature baseline and offset; the app uploads the ring's per-minute skin temperature
  (section 10.2) and then clears it from its database, so the backup has no temperatures.
- Deep, light and REM are scored in 5-minute blocks counted from sleep onset (99 % of blocks hold
  one stage); awake is scored per 2.5-minute epoch on top.
- Deep ends at a median 35 % of the sleep period (middle half 31 to 40 %) and makes up 15 to 27 %
  of sleep. Inside that window no stored reading separates deep from light well (best: HRV against
  the surrounding hour, AUC 0.62).
- REM is a block with heart rate high for that night and HRV low (heart rate rank AUC 0.81).
- On 5-minute blocks with an hour of context either side, a model matched 87 % (kappa 0.78).
- Fixed rules found in the scoring: the first block after sleep onset is always light (the next is
  deep 93 % of the time); the block after a waking is never deep; a block with a restless epoch is
  deep 11 % of the time. Inside the deep window, 29 % of motionless blocks are still light, and
  nothing stored explains which. Nights with an apnea recording score the same as nights without.
- REM episodes start a median 55 minutes apart with no regular cycle; REM has no autocorrelation
  beyond 20 minutes.
- A rule-based stager matched 80 % of 5-minute blocks on held-out nights (kappa 0.68): waking from
  the restless flag or a movement total over 2000; deep in the first 33 % of the sleep period when
  the block's movement total is 1200 or less; REM after 30 % from one logistic score on heart rate
  and HRV ranked within the night and against the surrounding four hours; then the order rules.
- The app's own logs (`files/log`, `files/onlineLog`) are encrypted.

**Nightly figures (RM, 2026-10-24).** Over Ringmaster's own sleep windows on the same 158 nights:
the median heart rate came within 0.85 bpm of the app's nightly average (MAE), the lowest average
over 6 consecutive epochs within 0.70 bpm of its minimum, the mean HRV within 1.13 ms, the mean
SpO2 within 0.31 points and the mean respiratory rate within 0.12 breaths/min. Time asleep (the
window less restless epochs, by the waking rule above) ran a median 23 minutes over the app's
sleep duration (MAE 37 minutes); the window itself tracks the app's time in bed (median 7 minutes
short). The app's resting heart rate is not a nightly minimum (3 to 4 bpm higher) and its lowest
SpO2 is smoothed (4 points above the lowest reading).

**Apnea reports in the official app.** Computed by the vendor's server from raw PPG recorded in
a separate monitoring mode; the database keeps per-hour event counts and 3-minute SpO2 minimum
and maximum, not the PPG. Per-hour events follow the 3-minute SpO2 spread (Spearman 0.64) but
epoch data predicts them weakly (0.52 hourly, 0.66 nightly), and the severity band matched on 53 %
of nights against 47 % for always guessing mild. The `0x47` samples are skin temperature
(section 10.2) and do not track SpO2.

Further detail (RM, 2026-10-22): monitoring is a mode the user starts (it needs at least 30 %
battery); the 79 reports come almost nightly in 2025-12 and 2026-01, then in runs of 3 to 5 nights.
On those nights the `0x4c` epochs are unchanged, and the app's tables (`OasStatusModel`:
`offlineData`, `ppgStartUtc`, `ppgEndUtc`, `ppgUrl`) point to a separate recording kept on the ring
and fetched later. In each report the AHI is events per effective monitoring hour and the ODI is
desaturations per hour; the desaturation count follows the number of 3-minute windows with a
spread of 3 points or more (Spearman 0.94), so the ODI counts 3 % dips in a SpO2 signal sampled
far faster than the epochs. The AHI runs a median 2.5 times the ODI, so most events are scored
without a desaturation. From epochs alone, hours with more events have higher HRV than the rest
of the night (Spearman 0.34) and more epoch-to-epoch change in heart rate. The four undecoded
fields of the live SpO2 frame (`<x:2> <y> <p:2> <q>`) match the app's unused SpO2 detail table in
shape (`acIr`, `piIr`, `acRl`, `piRl`: two AC values and two perfusion indices). (Unverified)

## 12. Operational behaviour

- **One shared resume pointer per channel.** Whoever syncs first gets the backlog. Running the
  official app on another phone will silently take data from Ringmaster. (OC, confirmed)
- **Draining repeatedly overnight (disputed).** OC lost whole nights by syncing every 90 minutes and
  puts the ring's buffer at about 19 days. Against that: the official Android app syncs every 15
  minutes (section 7.1), and RM's first sync after four months held exactly 7 days. OC's loss may
  have come from its own handling of pages; Ringmaster stores each page before acknowledging it.
  Watch for overnight gaps at short sync intervals. Live-telemetry fetches (`07 00 00`) do not move
  the pointer. (OC; RM)
- **The ring stops advertising seconds after disconnect** and wakes on motion or charger contact.
  A ring sitting on a charger may not advertise either. (OC, RL, probable)
- **Stale links.** GB reports links that die silently while still marked connected. Treat a
  missing heartbeat or descriptor as a dead link. (GB, probable)
- **Leftover notification sessions (RM, bluer 0.17).** When BlueZ drops a link without ending the
  notify stream, a reader task that keeps the stream alive keeps bluer's notification session alive
  too. bluer hands that session to the next `notify()` on the same characteristic without calling
  `StartNotify`, so the ring's notifications stay off: the link connects, and the status request
  gets no answer. Restarting the app cleared it. Readers are now stopped when a link is dropped, and
  a silent ring triggers a new Bluetooth session.
- **Cached versus advertising (RM, BlueZ 5.85).** When discovery starts, BlueZ reports a device it
  already knows as added straight away, from its cache, with no RSSI. Only an RSSI shows the ring is
  advertising now. Connecting to a ring known only from the cache failed twice.
- **Signal (RM).** At -98 dBm the ring was seen but never connected; at -62 dBm it connected.
- **ATT error `0x0e` (RM, seen once).** Shortly after the ring came off the charger (descriptor
  states `04`, then `01`), a poll write was rejected with ATT error `0x0e` ("Unlikely Error").

## 13. Verification status on the tested Gen 2

| # | Question | Status |
|---|---|---|
| 1 | GATT layout and DIS values match section 2 | Done: matches (section 2) |
| 2 | BlueZ `device.pair()` completes the Just Works bond | Done: bonds, but the call may never return; ring must be out of a closed case (section 3) |
| 3 | SM3 auth succeeds with the MAC from bluer | Done: accepted (section 5) |
| 4 | `07 00 00` or `d0 00 00` wakes telemetry | Done: `07`; `d0` unanswered (section 5) |
| 5 | Descriptor offsets | Done: confirmed (section 9.1) |
| 6 | Channel `02` (sport) exists on Gen 2 | **Open**: not reached |
| 7 | Epoch | Done: `1577793600` (section 7.5) |
| 8 | `82` reply bytes | Partly: `82 00 00 82` even for a channel that yields nothing (section 7.3); `82 ff` not yet seen |
| 9 | Live heart rate with the ring worn | Done: works with every start-up tried; SpO2 too (section 8) |

## 14. From the official app (decompiled 3.10.0 and 2.2.1)

Read from the Android app's compiled Dart code with Blutter and a disassembler (no vendor code
used). Names are obfuscated in these builds; meanings come from the app's own log messages.
Version 3.11.0 and later use Shorebird, which Blutter cannot read. Unverified on the tested ring unless
another section confirms it.

Replies the app's dispatcher handles, by first byte (a reply is its command with bit 7 set):

| Reply | App's log label | Command |
|---|---|---|
| `11` | event state sync | ring-initiated |
| `12` | real-time PPG | stream after `06 04` |
| `13` | real-time accelerometer | stream |
| `15` | live measurement | stream (section 8) |
| `18` | history heart rate | |
| `19` | history SpO2 | |
| `1c`, `43`, `44`, `47` | history activity | |
| `1e` | history measurement | |
| `40` | history measurement V2 | |
| `41` | history sport | |
| `42` | real-time sport | |
| `4a` | automatic sport | |
| `81` | authorisation | `01` |
| `82` | work mode | `02` |
| `83` | bind | `03` |
| `85` | parameter setting | `05` |
| `86` | real-time measurement | `06` |
| `87` | descriptor | `07` |
| `88` | system control | `08` |
| `89` | OTA (firmware) update | `09` |
| `8a` | offline apnea (OSA) data | `0a` |
| `a0` | leave factory mode | `20` |
| `a2` | PPG (factory test) | `22` |
| `a3` | accelerometer (factory test) | `23` |
| `a4` | LED (factory test) | `24` (section 6: LED) |
| `a9` | temperature | `29` |

The real-time measurement command `06` takes a mode: `01` heart rate and `02` SpO2 (as in
section 6), and `04` for the PPG stream the app records overnight for its apnea report (log:
"start real-time PPG collection, osaSn"). The app stores the stream as CSV files under `osa/`.

Frame `12` (from 2.2.1's handler, layout incomplete): samples from byte 3 up to six bytes before
the end, then four values and a byte holding two 4-bit gain codes (5 means x50, 2 means x5,
otherwise x1); values are scaled by a per-ring constant between 700,000 and 1,000,000 (default
773,333). `ringprobe --overnight` now tries `06 04 00` and keeps sample `12` frames in its report.

Tested 2026-10-27 on the tested Gen 2 (firmware FR02.018): `06 04 00` and OC's `06 05 00` are both
refused at once with `86 ff` (accepted modes answer `86 00`), and no `12` frames follow; `06 00 00`
afterwards is accepted as usual. The app may enable apnea
monitoring first, or append the session serial its log names (`osaSn`).

**Work mode.** The app's "work mode" command is the sync open of section 7:
`02 <mode> <cursor:4> <byte> 01 00` (the app builds mode, a 4-byte big-endian time, the byte or
`00`, then `01`). Mode `00` is the sync open; at connect the app sends mode `01` with its heartbeat
interval setting (default 3) in the byte slot, or mode `00`; when it starts collecting PPG ("start
collecting PPG data") it sends mode `02` with `00`. The likely sequence for the PPG stream is
`02 02 <cursor> 00 01 00`, then `06 04 00`; `ringprobe --overnight` now tries exactly that. Any
later sync open (mode `00`) is expected to return the ring to normal.

Tested 2026-10-27 11:36 (FR02.018): `02 02 <cursor> 00 01 00` is accepted (`82 00 00 82`) and
streams no history; the descriptor's state byte then reads `05`, a state not seen before (idle is
`02`/`03`, sport `06`), and stays `05` until disconnect. `06 04 00` and `06 05 00` are still
refused (`86 ff`). State `05` is probably the apnea recording mode, in which the ring may record
PPG itself for download with `0a` (reply `8a`, "request offline OSA data"); not yet confirmed.
After a Ringmaster sync (mode-`00` sync opens) the state read `02` again; whether the sync or the
disconnect before it ended state `05` is not known.

**Exploration run, 2026-10-27 12:22 (`ringprobe --explore 5`, FR02.018).** Every reply below is
in the report's frame log.

| Sent | Result |
|---|---|
| `06 03 00` | Accepted; state `06` (sport). One `4e` frame (`4e 0c d5 a5 a3 4b 00 00 00 ff 00 00 25`: cursor time, then `4b` = 75, likely heart rate), and the newest `4c` epoch pushed twice. So sport mode starts with `06 03 00` as well as OC's `06 03 07 04 00`. |
| `06 04` to `06 0a` (not `06`) | All refused (`86 ff`), in state `02` and (`04`, `05`) in state `05` |
| `29 00 00` | `a9 01 12 01 11 00 00 00 00 aa`: the two skin temperature fields, as in the status frame |
| `22 01 00`, `23 01 00` | Refused (`a2 ff`, `a3 ff`); the factory PPG and accelerometer tests need factory mode. `22 00 00`, `23 00 00` accepted |
| `0a` with 2, 6 and 7 payload bytes | No reply in any state |
| Sync open, channels `01`, `04` to `08` | Accepted (`82 00 00 82`), no pages |
| Work mode 2 | State `05`; 5 minutes worn brought no unprompted frame |
| Work mode 2 on channels `01`, `04`, `05` | Accepted, no pages |
| Work mode 0, then work mode 1, while connected | Accepted, but the state stayed `05` |

So on FR02.018 neither the live PPG modes nor the apnea data request are reachable as tried.

**Apnea monitoring switch (3.10.0).** Before its PPG command, the app's "start monitoring" flow
sends parameter `05` with id `22` and a value from an enum the app names `close` 0, `open` 1,
`fastClose` 2; at connect, with no monitoring running, it sends `close` or `fastClose` (log "ring
is ..., no osa"). Probable frame: `05 22 01 00` (switch on), `05 22 00 00` (off). Reply `85`. Other
apnea replies the app handles: `45` offline apnea data and `48` automatic apnea data, each with a
16-bit remaining count in bytes 1 to 2; `46` online apnea data, acknowledged with `c6`.
`ringprobe --osa MINUTES` tests this.

Tested 2026-10-27 13:23: `05 22 01 00`, `05 22 01`, `05 22 02 00` and `05 22 00 00` all answer
`85 22 00 a7` (id echoed, then `00`), with no change in state or behaviour; `06 04` still refused,
`0a` still unanswered, nothing sent unprompted over 4 minutes.

**Work mode 2 is factory mode; raw PPG comes from the factory PPG test (3.10.0).** The app's
blood-pressure flow logs "enter factory mode" around work mode 2, then sends `22 <mode>` and
`23 03`. For ring models the app calls `second` (Gen 2) and `secondAir` the PPG mode is `0a`
with a rate setting of 100; for others `08` with 25. It stops with `22 00`, `23 00` and leaves
factory mode with `20 01` (reply `a0`, "exit factory mode response"). So state `05` is factory
mode, which explains why sync opens did not end it. `22 01` was refused because `01` is not a
mode. Expected sequence for Gen 2: `02 02 <cursor> 00 01 00`, `22 0a 00`, stream (probably `12`
frames), `22 00 00`, `20 01 00`. `ringprobe --ppg SECONDS` runs exactly this.

**Raw PPG confirmed, 2026-10-27 14:10 (FR02.018).** Work mode 2 (state `05`), then:

| Sent | Result |
|---|---|
| `22 0a 00` | Refused (`a2 ff`) on this firmware |
| `23 03 00` | `a3 00`; accelerometer frames `13`, 160 bytes, about one a second |
| `22 08 00` | `a2 00`; PPG frames `12`, 135 bytes, about 2.5 a second, until `22 00 00` |
| `20 01 00` | `a0 00`; state `01` (settling), then normal |

Frame `12`: `12 00 <seq> 01 00 83`, then ten 12-byte samples, each four 24-bit big-endian
channels, then 9 bytes: `0a <gain 0a/0c/0f> 00 0a 00 <count:2> <checksum>`, the count rising by 10
per frame. So 25 samples a second. Channel 1 carries the pulse (6 s of data: autocorrelation peak
at 22 samples, r 0.8, 68 bpm; the ring's own live reading at the same moment was 65). Channel 2
is a second wavelength with little pulse at this gain, channel 3 a small positive signal (likely
ambient), channel 4 a signed 20-bit value near zero (likely ambient-subtracted or offset).

Frame `13`: `13 00 <seq> 01 00 9d`, then 6-byte samples of three signed 16-bit big-endian axes
(about `0002 ff7b fc35`: x 2, y -133, z -971, so 1 g is about 1000), then a tail.

Frames `15` (`15 10 41 0a b0 fe`: `41` = 65, a heart rate) and `16` (`16 <cursor:4> 41 ...`) also
arrived while the PPG test ran.

**Session length (2026-10-27 14:25).** Asked for 600 s, the PPG test streamed 35 frames (13.5 s)
and stopped by itself; the ring went from state `05` back to `02` about 20 s after work mode 2,
leaving factory mode without `20 01`. Live heart-rate frames `15` (62 to 65) ran alongside, and a
`16` frame followed the end of the session. Over those 350 samples channels 1 and 2 both pulse at
65 bpm (autocorrelation r 0.86 and 0.83); AC/DC is 0.25 % on channel 1 and 0.15 % on channel 2,
a ratio of 0.59, which in the usual red/infrared formula (110 - 25R) gives 95 %. So channel 2 is
probably red and channel 1 infrared. The app's log "resending the PPG command while already
started" suggests it restarts sessions; `ringprobe --ppg` now does that whenever frames stop.

**Chained sessions (2026-10-27 14:44, `--ppg 600`).** Restarting with work mode 2 and `22 08 00`
whenever frames stop for 2.5 s works: sessions lasted 35, 41, 41, 67 and 105 s (longer each
time), restarts cost about 3.6 s, and frames covered 96 % of the 302 s until the link dropped
(writes went on succeeding; the ring had likely locked; see "Chained sessions lock the ring" below). The ring's own live heart rate (`15` frames every
2 s, 58 to 98) ran alongside throughout. Pulse quality was good in the first minute (both
channels 60 bpm) and poor afterwards, with channel 3 saturating (`07 ff fe`) from about 180 s;
the wearer was working at a desk, so motion is the likely cause. Needs a still test.

**Still hand (2026-10-27 15:00, `--ppg 300`).** Five sessions (42, 38, 42, 42, 123 s), restarts
3.2 to 3.8 s, 96 % coverage, no saturation, link stable. Spectral heart rate on 10 s windows was
60 bpm on both channels in most windows, against the ring's own 56 to 63; the rest were
harmonic errors of the short window. The red/infrared ratio varied (implied SpO2 81 to 96 %), so
SpO2 from these channels needs calibrating against the ring's own readings.

**Chained sessions lock the ring (2026-10-27 22:12, `--overnight 8 --raw-ppg`).** Sessions
restarted with work mode 2 and `22 08 00`, never leaving with `20 01`, ran for 685 s. Then the
write of the next work-mode command failed with ATT error `0x0e` and the link dropped. For the
remaining 7.6 h the ring took 211 connections, and on 183 of them notifications were enabled, but
it answered no command, not even `01 00 00`. It came back as soon as it was seated in its case.
Its own recording carried on throughout: the next sync held the whole night. The 14:44 run above
ended the same way: at 323 s every frame stopped mid-session, PPG and live heart rate alike,
with no command sent since the session began, and the run stopped at 625 s.

**No lock-up in 21.6 min (2026-10-28 07:52, `--overnight 0.35 --raw-ppg`, same procedure, no
`20 01`).** 32 sessions of 37 to 39 s, restarts 2.9 s apart, 94 % coverage, no reconnects.
Spectral heart rate on 10 s windows had a median of 64 bpm against the ring's own 62, and
channel 3 never saturated. So the lock-up is not a fixed time limit. Both runs that locked
started with the ring in state `03`; this one started in `02`. Three runs are too few to say
whether that matters.

**Leaving factory mode after each session (2026-10-28 08:36, `--overnight 1 --raw-ppg`, 5 s
rest).** No lock-up in 60 min: every reconnect that got through was answered. `20 01 00` got
`a0 00` and state `01` each time it could be sent. After the first two sessions (6 and 36 s),
sessions ran far longer than before: 1014 s, 388 s and 536 s of unbroken PPG, besides runs of
36 to 42 s. Four times the link dropped (ATT error `0x0e` on the next write) as a session's
frames stopped, and for the next 4 to 5 min no connection got through. The wearer had walked away
from the phone each time, so these were the ring going out of range, not a fault; on return it
reconnected and answered at once. So ATT error `0x0e` on a write is what a link lost to range
looks like, and also what preceded the overnight lock-up. PPG
covered about 73 % of the hour; spectral heart rate had a median of 60 bpm against the ring's
57, with the wearer at work. Whether leaving factory mode prevents the lock-up is still open:
the procedure without it also ran 21.6 min cleanly.

**The ring records nothing of its own while PPG streams (2026-10-28 21:12, `--overnight 8
--raw-ppg`).** Sessions left with `20 01 00` and rested 5 s. The signal was -93 dBm throughout.
Two sessions streamed unbroken for 40 min (21:21 to 22:02) and 29 min (22:08 to 22:37); spectral
heart rate had a median of 62 bpm against the ring's 60, with 95 % of 10 s windows in a plausible
range. After the third link drop the ring took connections but answered none for 3.5 min, and the
run stopped; Ringmaster then connected without the case, so the ring had not locked and the weak
link was the likely cause (the run now waits 20 min of unanswered connections before stopping).
The next sync showed no history from 21:12 to 22:37, the span from the first work-mode-2 command
to the last PPG frame: in factory mode the ring keeps none of its 2.5-minute records. Raw PPG
therefore replaces the ring's own measurements for the time it runs rather than adding to them. Ringmaster
reports a ring that connects but does not answer even over a new Bluetooth session.

