# Ringmaster conventions

- App ID: `net.dunevent.Ringmaster`
- License: GPL-3.0-or-later
- Stack: Rust, GTK4, libadwaita; target FuriLabs FLX1s, FuriOS (Debian forky), Phosh, BlueZ 5.85
- Workspace: `crates/ringconn`, `crates/colmi` and `crates/rwfit` (protocols, no I/O), `crates/ringmaster` (app), `tools/ringprobe`
- Other brands' protocols: facts from GPL or AGPL projects (Gadgetbridge) and from PulseLoop (CC BY 4.0, credited in the docs that use it); no code copied from any of them
- Code comments follow "Comment Style Guide 2026-09-21"
- Source on the project forge: https://dev.dunevent.net/aj/Ringmaster (git, `main`); the user commits and pushes
- Deliveries: zip files containing only changed or new files, with directory structure preserved under the project root `Ringmaster/`. Deleted files are listed in the message.
- `.no-sync` files list the direct children of their folder that device sync should skip (build output), one name per line, like `.hidden`. A folder whose own children need skipping gets its own `.no-sync`. Keep them current when a build step adds an output folder.
- README is a landing page: features and how to use the app. Building, layout, tests and tools go in `docs/DEVELOPMENT.md`; ringprobe is not mentioned in the README
- Docs name hardware only as "Tested on …", never purchases or what devices the user owns
