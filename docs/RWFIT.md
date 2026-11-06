# RWfit rings (RWFit app)

Rings whose maker points to the RWFit app (`com.rw.revivalfit`), sold under many brands, some as
"Colmi" and "R12". They share nothing with the Colmi protocol in `docs/COLMI.md`.

Facts here come from PulseLoop (CC BY 4.0, by Saksham Bhutani,
https://github.com/saksham2001/PulseLoopiOS, commit 8d8d8b6 of 2026-10-29), whose RWfit driver was
reconstructed from the vendor app, and from the vendor's own published SDK it cites
(https://github.com/RWFitSDK/RW_weixi_miniprogram_sdk). No code is copied. Nothing here has been
checked against a ring yet.

## Recognition

No name matching: rebranders change the name. A ring is RWfit when its advertisement carries

- the 16-bit service `A00A` (vendor pattern `02 01 06 03 03 0a a0`), or
- manufacturer data starting `d6 05 02 00`, `d6 05 41 54` (company `0x05D6`) or `d6 06 02 00`
  (company `0x06D6`, the "T-Ring" line).

**R12 (tested 2026-11-03).** Advertises as "SR16" from a public address, manufacturer data
`0x06D6`: `02 00 81 00 52 <address:6> 80 43 44 33 30 33 38 30 30 30 30` (its own address at bytes 5
to 10, then ASCII "CD3038" and zeros). Connected without bonding, it offers the standard
Heart Rate service (`180D`) and `A00A`, and none of the JieLi hint services, so the legacy framing
is expected. `ringprobe --rwfit` reads it.

From a desktop (2026-11-04), BlueZ resolved the full table before the link dropped: `0BC0` (vendor; `0BC1` write and notify, `0BC2`
read and notify), `1812` HID over GATT (report map, several input and output reports, protocol
mode, HID control point: the likely route for the gestures), `A00A` (`B002` write, `B003`
notify), `FF00` (PixArt OTA; `FF01` to `FF03`), and GAP and GATT. `FF00` is one of the JieLi hints,
so the JieLi framing is now the more likely one; the first validated frame decides.

On the FLX1s (BlueZ 5.85) the R12 is dropped about 2 s after every connection, bonded or not, and
on a desktop after about 6 s:
it sends a second ATT request before the first is answered, and BlueZ closes its ATT channel.
`docs/bluez/` has the cause in full and a BlueZ patch that tolerates it.

## GATT

| UUID | Use |
|---|---|
| `0000a00a-…` | Data service, both framings |
| `0000b002-…` | Write (with or without response) |
| `0000b003-…` | Notify |
| `0000ae00-…` (JieLi), `00010203-0405-0607-0809-0a0b0c0d1912` (Telink OTA), `0000ff00-…` (PixArt OTA) | Present only on JieLi-framing rings; a hint, confirmed by the first frame that passes its CRC |

All multi-byte fields are big-endian.

## Legacy framing (`0x7E`, "Realtek")

`7E 01 <cmd> <flags> <len> <serial:2> <xor> <payload>`. The XOR is over the payload (0 when empty).
Flags bit 3 marks a multi-packet frame, which inserts `<total:2> <current:2>` (1-based) before the
payload; each chunk has its own length and XOR. Serials run 1 to 65535 and wrap; a new link accepts
any serial.

Every frame is acknowledged, both ways, before it is parsed:

- the ring acknowledges a command with cmd `FE`, payload `<serial:2> <cmd> <status>`;
- the app acknowledges each ring frame (each chunk) with cmd `FF`, same payload with the ring's
  serial; status 0 is OK, 2 asks for a retransmission after a bad XOR.

Commands: `00` device info, `01` battery, `02` bind status, `03` feature bitmap, `20` bind, `21` set
time, `24` units, `2E` profile, `44` unbind; history `A0` manifest, `A1` steps, `A2` sleep, `A3`
heart rate, `A4` blood pressure, `A5` SpO2, `A6` temperature, `A7` breathing. Record times are local
wall-clock Unix seconds.

## JieLi framing (`0xAB`)

`AB <flag> <len:2> <crc:2> <payload>`, CRC-16/ARC (reflected polynomial `0xA001`, initial 0, no
final XOR) over the payload. A payload longer than one notification continues in packets with no
header until `len` bytes have arrived. Flag `01` is a request or push, `11` a reply; `21` also
occurs. The app acknowledges each flag-`01` frame from the ring with flag `11` and the echoed
three-byte triple; replies and pushes are not acknowledged; a bad CRC is dropped silently.

The payload starts with a `<cmd> <key> <keyflag>` triple (keyflag `00` set, `10` get or sync, `20`
bind, `30` unbind):

| Triple | Meaning |
|---|---|
| `02 01 00` | Set time |
| `02 03 10` | Battery |
| `02 04 10` | Device info |
| `02 06 00`, `02 07 00`, `02 11 00` | Profile, goal, units |
| `03 01 00` / `20` / `30` | Bind status / bind / unbind |
| `05 <type> 10` | History for one type |
| `06 09 00 <type> 05 <on>` | Live measurement on or off |

Types: `02` steps, `03` heart rate, `04` blood pressure, `05` sleep, `08` temperature, `09` SpO2,
`0A` HRV, `0D` stress, `10` blood sugar, `1A` today's steps. Record times are local wall-clock
seconds since 2000-01-01.

The feature menu (reply `02 63`) gives one flag bit per stream at fixed offsets counted from the
start of the triple: password needed at `0x2C`; history present at `0x53`, then steps `0x54`, heart
rate `0x55`, blood pressure `0x56`, sleep `0x57`, SpO2 `0x59`, HRV `0x5A`, stress `0x5B`, blood
sugar `0x5C`, temperature `0x5E`.

## Not yet read

Record layouts for each history type, the bind sequence, and the order of the vendor's startup
commands are in PulseLoop's `RWfitDecoder.swift`, `RWfitHistorySync.swift` and
`RWfitSyncEngine.swift`; they go here when a ring advertises and the framing is known.
