use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icons/yututui.ico");
    // Embedded by the About card via `include_bytes!`; rebuild if the icon changes.
    println!("cargo:rerun-if-changed=assets/icons/yututui-about.png");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let icon = manifest_dir
        .join("assets")
        .join("icons")
        .join("yututui.ico")
        .display()
        .to_string()
        .replace('\\', "/");
    // Icon + VERSIONINFO. Windows shell surfaces (Task Manager, the media flyout's
    // identity fallbacks) read FileDescription/ProductName off the exe when nothing
    // better is registered (see src/media/identity.rs). One crate-wide resource links
    // into BOTH bins, so the strings stay binary-neutral (no OriginalFilename), and
    // the FILEVERSION digits derive from CARGO_PKG_VERSION so they can't rot.
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut nums = version.split('.').map(|part| {
        part.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u16>()
            .unwrap_or(0)
    });
    let (major, minor, patch) = (
        nums.next().unwrap_or(0),
        nums.next().unwrap_or(0),
        nums.next().unwrap_or(0),
    );
    let rc = out_dir.join("yututui.rc");
    let rc_source = format!(
        r#"1 ICON "{icon}"

1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "YuTuTui!"
      VALUE "ProductName", "yututui"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "CompanyName", "Ochichan"
      VALUE "LegalCopyright", "MIT License"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    std::fs::write(&rc, rc_source).expect("failed to write Windows resource script");

    embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed Windows icon resource");

    // PerMonitorV2 DPI manifest, scoped to the `yututray` bin only: the mini player's
    // WebView2 renders crisply on mixed-DPI setups. Passed as a per-bin LINKER input —
    // MSVC merges every /MANIFESTINPUT with rustc's default manifest — because a
    // crate-wide `1 24` resource would leak into `ytt.exe` and risk duplicate-manifest
    // link errors.
    let manifest = out_dir.join("yututray.manifest");
    std::fs::write(&manifest, DPI_MANIFEST).expect("failed to write yututray manifest");
    println!("cargo:rustc-link-arg-bin=yututray=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bin=yututray=/MANIFESTINPUT:{}",
        manifest.display()
    );
}

const DPI_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2, PerMonitor</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>
"#;
