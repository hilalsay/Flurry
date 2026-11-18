//! `flurry --install-autostart` / `--remove-autostart`: a freedesktop
//! autostart entry (`~/.config/autostart/flurry.desktop`), honoured by
//! Cinnamon, GNOME, KDE, XFCE and MATE.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const FILE_NAME: &str = "flurry.desktop";

/// `$XDG_CONFIG_HOME/autostart` (usually `~/.config/autostart`).
pub fn dir() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().context("cannot find the home directory")?;
    Ok(base.config_dir().join("autostart"))
}

/// The desktop entry that starts `exe` at login.
pub fn entry(exe: &Path) -> String {
    // Desktop-entry Exec values quote arguments with double quotes.
    let exe = exe.display().to_string();
    let exec = if exe.contains(' ') {
        format!("\"{}\"", exe.replace('"', "\\\""))
    } else {
        exe
    };
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Flurry\n\
         Comment=Snow falling over your desktop\n\
         Exec={exec}\n\
         Icon=weather-snow\n\
         Terminal=false\n\
         Categories=Utility;\n\
         X-GNOME-Autostart-enabled=true\n\
         X-GNOME-Autostart-Delay=5\n"
    )
}

/// Write the entry into `dir`; returns its path.
pub fn install(dir: &Path, exe: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = dir.join(FILE_NAME);
    std::fs::write(&path, entry(exe))
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

/// Remove the entry from `dir`; returns whether there was one.
pub fn remove(dir: &Path) -> Result<bool> {
    let path = dir.join(FILE_NAME);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("cannot remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_and_remove() {
        let dir = std::env::temp_dir().join(format!("flurry-autostart-{}", std::process::id()));
        let path = install(&dir, Path::new("/home/me/My Apps/flurry")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("[Desktop Entry]\n"));
        assert!(
            text.contains("Exec=\"/home/me/My Apps/flurry\"\n"),
            "{text}"
        );
        assert!(remove(&dir).unwrap());
        assert!(!remove(&dir).unwrap(), "already gone");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
