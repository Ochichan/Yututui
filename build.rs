use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icons/ytm-tui.ico");
    // Embedded by the About card via `include_bytes!`; rebuild if the icon changes.
    println!("cargo:rerun-if-changed=assets/icons/ytm-tui.png");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let icon = manifest_dir
        .join("assets")
        .join("icons")
        .join("ytm-tui.ico")
        .display()
        .to_string()
        .replace('\\', "/");
    let rc = out_dir.join("ytm-tui.rc");
    std::fs::write(&rc, format!("1 ICON \"{icon}\"\n"))
        .expect("failed to write Windows resource script");

    embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed Windows icon resource");
}
