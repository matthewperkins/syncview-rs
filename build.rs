//! The git commit being built, as SYNCVIEW_COMMIT (shown by `--version` and in the key
//! reference), "unknown" outside a git checkout.
//!
//! Windows: embed assets/icon.ico in syncview.exe, so Explorer, the Start menu and the taskbar
//! show the app's icon (the running window's icon is set separately, in main.rs).

fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    // rerun when HEAD moves (a commit, checkout or pull changes HEAD or the index)
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/index");
    }
    println!("cargo:rustc-env=SYNCVIEW_COMMIT={}", git(&["rev-parse", "--short=7", "HEAD"]).unwrap_or_else(|| "unknown".into()));
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.compile().expect("embedding the Windows icon");
    }
}
