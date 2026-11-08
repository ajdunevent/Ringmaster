# Developing Ringmaster

## Build and run

```sh
sudo apt install build-essential pkg-config libdbus-1-dev libgtk-4-dev libadwaita-1-dev
cargo run --release
```

`--hidden` starts it without a window, as at login. Set `RUST_LOG=ringmaster=trace` to log every
frame sent and received. Data lives in `~/.local/share/ringmaster/ringmaster.db`.

## Flatpak

`build-aux/dev.sh` asks for the architecture (this machine, the other one of x86_64 and aarch64,
or both), whether to write a `.flatpak` file to `dist/`, install it for the current user, or both,
and which build profile to use: fast release (the `release-dev` profile: optimised, no LTO,
incremental), debug, or release as published. It keeps the crate cache and each profile's compiled
output in `.flatpak/`, so a second build recompiles only what changed. Building for the other
architecture needs QEMU user emulation registered with binfmt_misc (`qemu-user-static`), and is
slow.

`build-aux/cargo-sources.json` lists every crate for the offline build. Regenerate it whenever
`Cargo.lock` changes, with `flatpak-cargo-generator.py Cargo.lock -o build-aux/cargo-sources.json`
from [flatpak-builder-tools](https://github.com/flatpak/flatpak-builder-tools). Under Flatpak the
database is in `~/.var/app/net.dunevent.Ringmaster/data/ringmaster/`, and start at login goes
through the Background portal.

## Distrobox

Inside a Distrobox container, Ringmaster and ringprobe find the host's Bluetooth service on their
own: distrobox leaves the host's system bus out of the container, so they use it from `/run/host`.
The container needs the build packages (on Fedora: `gcc pkgconf-pkg-config dbus-devel gtk4-devel
libadwaita-devel`). A ring paired with the phone has to be paired again on the computer, and only
one device should sync it.

## Layout

| Path | Contents |
|---|---|
| `crates/ringconn` | RingConn protocol without I/O: framing, authentication, record decoding, the history drain state machine |
| `crates/colmi` | Colmi protocol without I/O: packets, requests, history and big-data decoding |
| `crates/rwfit` | RWfit protocol without I/O: recognition, both framings, record decoding |
| `crates/ringmaster` | The app: BlueZ transport, SQLite store, backend thread, GTK window |
| `build-aux/` | Flatpak manifest, vendored crate list and `dev.sh` |
| `.forgejo/workflows/`, `.github/workflows/` | Tagging on the forge, building and releasing on GitHub |
| `tools/ringprobe` | Diagnostic probe: records a ring's answers in one text report; see its README |
| `docs/PROTOCOL.md` | The RingConn protocol as currently understood, with sources and confidence for each fact |
| `docs/COLMI.md`, `docs/RWFIT.md`, `docs/ULTRAHUMAN.md` | Notes on other brands, with sources |
| `docs/bluez/` | Why BlueZ drops RWfit rings, with a patch for BlueZ 5.85 |
| `docs/project/` | Conventions and the backlog |
| `.no-sync` | Build output that device sync should skip: one name per line, direct children of this folder only |

`crates/ringconn/tests/fixtures` holds real history pages from a tested ring.

## Tests

```sh
cargo test
```

The replay tests drive the drain state machine against a simulated ring that serves real Gen 2
pages with the fetch and ACK behaviour recorded from hardware.

## Releasing

Change the version in `Cargo.toml`, add a `<release>` at the top of `<releases>` in
`data/net.dunevent.Ringmaster.metainfo.xml`, and push to `main`. `.forgejo/workflows/tag.yml`
then tags the commit `v<version>` on the forge, or fails without tagging if the two versions
differ. The push mirror carries the tag to GitHub, where `.github/workflows/release.yml` runs the
[FrogPak releaser](https://github.com/FrogPak/releaser): it builds
`net.dunevent.Ringmaster-x86_64.flatpak` and `net.dunevent.Ringmaster-aarch64.flatpak` and
publishes a GitHub release with both, which FuriHub and AJHub pick up.

`Cargo.lock` does not need updating by hand for a version change; the Flatpak build updates the
workspace crates' versions in it offline.

Tags must not be made on GitHub: the mirror deletes any tag the forge does not have, and GitHub
then turns that tag's release into a draft, which the Ponds drop.
