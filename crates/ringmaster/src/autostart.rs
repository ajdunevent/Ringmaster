//! Start at login. A Flatpak cannot write its own autostart entry and asks through the Background
//! portal; outside Flatpak the entry is written directly.

use anyhow::{Context, Result};
use std::path::PathBuf;

const DESKTOP_ID: &str = "net.dunevent.Ringmaster.desktop";

pub fn in_flatpak() -> bool {
    std::path::Path::new("/.flatpak-info").exists()
}

fn entry_path() -> PathBuf {
    glib::user_config_dir().join("autostart").join(DESKTOP_ID)
}

pub async fn set(enabled: bool) -> Result<()> {
    if in_flatpak() {
        let response = ashpd::desktop::background::Background::request()
            .reason("Start Ringmaster hidden at login to keep the ring synced")
            .auto_start(enabled)
            .command(["ringmaster", "--hidden"])
            .dbus_activatable(false)
            .send()
            .await
            .context("the Background portal is unavailable")?
            .response()
            .context("start at login was refused")?;
        if enabled && !response.auto_start() {
            anyhow::bail!("start at login was refused");
        }
        return Ok(());
    }
    let path = entry_path();
    if !enabled {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).context("cannot remove the autostart entry"),
            _ => Ok(()),
        };
    }
    let exe = std::env::current_exe().context("cannot find the Ringmaster executable")?;
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=Ringmaster\nExec=\"{}\" --hidden\nIcon=net.dunevent.Ringmaster\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
        exe.display()
    );
    std::fs::create_dir_all(path.parent().unwrap()).context("cannot create the autostart folder")?;
    std::fs::write(&path, entry).context("cannot write the autostart entry")
}
