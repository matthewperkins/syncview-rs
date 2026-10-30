//! `syncview --install-desktop` (Linux and other freedesktop systems): add syncview to the
//! application launcher for this user, with its icon. The file is named after the window's
//! Wayland app id / X11 class (`syncview`), so docks and compositors match the window to it.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Write ~/.local/share/applications/syncview.desktop and the icon, pointing at this executable.
pub fn install() -> Result<()> {
    if cfg!(any(target_os = "macos", windows)) {
        anyhow::bail!(
            "--install-desktop is for Linux. macOS: drag syncview.app (from the release zip) to Applications. \
             Windows: right-click syncview.exe → Pin to Start."
        );
    }
    let exe = std::env::current_exe().context("finding this program's path")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let data = data_home()?;
    let icon = data.join("icons/hicolor/512x512/apps/syncview.png");
    let entry = data.join("applications/syncview.desktop");
    for p in [&icon, &entry] {
        std::fs::create_dir_all(p.parent().unwrap()).with_context(|| format!("creating {}", p.parent().unwrap().display()))?;
    }
    std::fs::write(&icon, crate::ICON_PNG).with_context(|| format!("writing {}", icon.display()))?;
    std::fs::write(&entry, desktop_entry(&exe)).with_context(|| format!("writing {}", entry.display()))?;
    eprintln!("syncview: wrote {}", entry.display());
    eprintln!("syncview: wrote {}", icon.display());
    eprintln!("syncview: the launcher runs {}; if you move or rebuild it elsewhere, run --install-desktop again.", exe.display());
    eprintln!("syncview: to remove: rm {} {}", shell_quote(&entry), shell_quote(&icon));
    Ok(())
}

/// $XDG_DATA_HOME, else ~/.local/share.
fn data_home() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_DATA_HOME").filter(|d| Path::new(d).is_absolute()) {
        return Ok(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/share"))
}

fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]
Type=Application
Name=syncview
GenericName=Electrophysiology and video viewer
Comment=View Open Ephys recordings frame-locked to a behaviour video
Exec={} %f
Icon=syncview
Terminal=false
Categories=Science;Biology;
Keywords=Open Ephys;EMG;electrophysiology;video;
StartupWMClass=syncview
",
        exec_quote(&exe.to_string_lossy())
    )
}

/// Quote a path for an Exec= line (Desktop Entry spec): inside double quotes, `"`, `` ` ``, `$`
/// and `\` are backslash-escaped, then every `\` is doubled again because the value is a string.
fn exec_quote(s: &str) -> String {
    let mut q = String::from("\"");
    for c in s.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            q.push('\\');
        }
        q.push(c);
    }
    q.push('"');
    q.replace('\\', "\\\\").replace('%', "%%")
}

fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"))
}

#[test]
fn exec_quoting() {
    assert_eq!(exec_quote("/usr/bin/syncview"), "\"/usr/bin/syncview\"");
    assert_eq!(exec_quote("/a b/$x\"y"), "\"/a b/\\\\$x\\\\\"y\"");
    assert_eq!(exec_quote("/100%/s"), "\"/100%%/s\"");
}
