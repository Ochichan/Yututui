fn main() {
    println!("cargo:rerun-if-changed=assets/icons/ytm-tui.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let host = std::env::var("HOST").unwrap_or_default();
    if !host.contains("windows") {
        println!("cargo:warning=skipping Windows icon embedding on non-Windows host {host}");
        return;
    }

    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icons/ytm-tui.ico")
        .set("FileDescription", "ytm-tui")
        .set("ProductName", "ytm-tui")
        .set("InternalName", "ytt.exe")
        .set("OriginalFilename", "ytt.exe");
    res.compile().expect("failed to embed Windows resources");
}
