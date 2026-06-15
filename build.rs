//! Embed the Windows app icon (assets/icon.ico) into the executable so it shows
//! in Explorer, the taskbar and Alt-Tab. Regenerate the .ico with
//! `python tools/make_icon.py`. Non-fatal: if the resource compiler is missing,
//! we warn and keep building (the runtime window icon is set in main.rs anyway).

fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/icon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed app icon: {e}");
        }
    }
}
