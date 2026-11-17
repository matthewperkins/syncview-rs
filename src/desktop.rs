//! `syncview --install-desktop` (Linux and other freedesktop systems): add syncview to the
//! application launcher for this user, with its icon. The file is named after the window's
//! Wayland app id / X11 class (`syncview`), so docks and compositors match the window to it.
//!
//! `syncview --install-app` (macOS): put this build into syncview.app in /Applications (or
//! ~/Applications), made as the release workflow makes it, so Spotlight and the Dock run it.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Write ~/.local/share/applications/syncview.desktop and the icon, pointing at this executable.
pub fn install() -> Result<()> {
    if cfg!(any(target_os = "macos", windows)) {
        anyhow::bail!(
            "--install-desktop is for Linux. macOS: syncview --install-app (or drag syncview.app from the release zip \
             to Applications). Windows: right-click syncview.exe → Pin to Start."
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

/// Make (or update) syncview.app from this executable: in `dest` if given, else /Applications,
/// or ~/Applications when /Applications can't be written.
pub fn install_app(dest: Option<PathBuf>) -> Result<()> {
    let mac = cfg!(target_os = "macos");
    if !mac && dest.is_none() {
        anyhow::bail!("--install-app is for macOS (on Linux: --install-desktop)");
    }
    let exe = std::env::current_exe().context("finding this program's path")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let app = match dest {
        Some(d) => d,
        None => {
            let system = PathBuf::from("/Applications");
            if writable(&system) {
                system.join("syncview.app")
            } else {
                PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join("Applications/syncview.app")
            }
        }
    };
    let bin = app.join("Contents/MacOS/syncview");
    if exe == bin.canonicalize().unwrap_or_default() {
        anyhow::bail!("this is already the app's own copy; run --install-app from the build (target/release/syncview or the cargo-installed one)");
    }
    let res = app.join("Contents/Resources");
    std::fs::create_dir_all(bin.parent().unwrap()).with_context(|| format!("creating {}", app.display()))?;
    std::fs::create_dir_all(&res)?;
    // the program: copied next to the old one, then renamed over it (fine while the app runs)
    let tmp = bin.with_extension("new");
    std::fs::copy(&exe, &tmp).with_context(|| format!("copying {} into {}", exe.display(), app.display()))?;
    std::fs::rename(&tmp, &bin)?;
    let plist = include_str!("../packaging/macos/Info.plist").replace("VERSION", env!("CARGO_PKG_VERSION"));
    std::fs::write(app.join("Contents/Info.plist"), plist)?;
    if mac {
        let icns = res.join("syncview.icns");
        if !icns.exists()
            && let Err(e) = make_icns(&icns)
        {
            eprintln!("syncview: no app icon ({e:#}); the app works without it");
        }
        // ad-hoc signature, as the release build has: without it a changed app may not open
        let ok = std::process::Command::new("codesign").args(["--force", "-s", "-"]).arg(&app).status().is_ok_and(|s| s.success());
        if !ok {
            eprintln!("syncview: codesign failed; if macOS won't open the app, run: codesign --force -s - {}", shell_quote(&app));
        }
        // so Spotlight and the Dock notice the new version straight away
        let _ = std::process::Command::new("touch").arg(&app).status();
    }
    eprintln!("syncview: installed syncview {} as {}", env!("CARGO_PKG_VERSION"), app.display());
    eprintln!("syncview: after the next git pull and build, run --install-app again to update it.");
    Ok(())
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".syncview-write-test-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// The app icon from assets/icon.png, as the release workflow makes it (sips + iconutil).
fn make_icns(out: &Path) -> Result<()> {
    let dir = std::env::temp_dir().join(format!("syncview-icon-{}", std::process::id()));
    let set = dir.join("syncview.iconset");
    std::fs::create_dir_all(&set)?;
    let png = dir.join("icon.png");
    std::fs::write(&png, crate::ICON_PNG)?;
    let run = |cmd: &mut std::process::Command| -> Result<()> {
        let s = cmd.stdout(std::process::Stdio::null()).status().context("running sips / iconutil")?;
        anyhow::ensure!(s.success(), "{cmd:?} failed");
        Ok(())
    };
    for s in [16u32, 32, 128, 256, 512] {
        for (scale, name) in [(1, format!("icon_{s}x{s}.png")), (2, format!("icon_{s}x{s}@2x.png"))] {
            let px = s * scale;
            if px > 1024 || (scale == 2 && px > 512) {
                continue;
            }
            run(std::process::Command::new("sips").args(["-z", &px.to_string(), &px.to_string()]).arg(&png).arg("--out").arg(set.join(name)))?;
        }
    }
    run(std::process::Command::new("iconutil").args(["-c", "icns"]).arg(&set).arg("-o").arg(out))?;
    let _ = std::fs::remove_dir_all(&dir);
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
