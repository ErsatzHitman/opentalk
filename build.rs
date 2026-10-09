//! Embeds the app icon, the application manifest (PerMonitorV2 DPI, asInvoker, Win10/11,
//! long paths, UTF-8 code page) and version info into `opentalk.exe`.
//! The resource script is generated here so the version always follows Cargo.toml.
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/opentalk.ico");
    println!("cargo:rerun-if-changed=assets/opentalk.manifest");
    println!("cargo:rerun-if-env-changed=OPENTALK_NO_RESOURCES");
    // OPENTALK_NO_RESOURCES=1 skips embedding (needed once to bootstrap `cargo run --example make_icon`
    // before assets/opentalk.ico exists, or when windres is unavailable).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") || std::env::var_os("OPENTALK_NO_RESOURCES").is_some() {
        return;
    }
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let slash = |p: PathBuf| p.to_string_lossy().replace('\\', "/");
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let part = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
    let (maj, min, pat) = (part("CARGO_PKG_VERSION_MAJOR"), part("CARGO_PKG_VERSION_MINOR"), part("CARGO_PKG_VERSION_PATCH"));

    let rc = format!(
        r#"1 ICON "{icon}"
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {maj},{min},{pat},0
PRODUCTVERSION {maj},{min},{pat},0
FILEFLAGSMASK 0x3f
FILEFLAGS 0
FILEOS 0x40004
FILETYPE 1
FILESUBTYPE 0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "OpenTalk"
      VALUE "FileDescription", "OpenTalk - Voice Dictation"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "opentalk"
      VALUE "OriginalFilename", "opentalk.exe"
      VALUE "ProductName", "OpenTalk"
      VALUE "ProductVersion", "{version}"
      VALUE "LegalCopyright", "OpenTalk"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon = slash(root.join("assets/opentalk.ico")),
        manifest = slash(root.join("assets/opentalk.manifest")),
    );
    let rc_path = out.join("opentalk.rc");
    std::fs::write(&rc_path, rc).expect("write opentalk.rc");
    embed_resource::compile(&rc_path, embed_resource::NONE).manifest_required().expect("embed Windows resources (windres on PATH?)");
}
