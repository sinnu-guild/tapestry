//! Embeds the app icon into tapestry.exe, so Explorer, desktop shortcuts and
//! the taskbar all show it. Windows-only; other platforms skip this.

fn main() {
    println!("cargo:rerun-if-changed=assets/tapestry.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/tapestry.ico");
    res.set("ProductName", "Tapestry");
    res.set("FileDescription", "Tapestry - Tapo camera wall");
    // A missing resource compiler shouldn't stop the app from building.
    if let Err(e) = res.compile() {
        println!("cargo:warning=couldn't embed the app icon ({e}); building without it");
    }
}
