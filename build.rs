//! Windows: embed assets/icon.ico in syncview.exe, so Explorer, the Start menu and the taskbar
//! show the app's icon (the running window's icon is set separately, in main.rs).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.compile().expect("embedding the Windows icon");
    }
}
