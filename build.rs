//! Embeds the app icon, the application manifest (PerMonitorV2 DPI, asInvoker, Win10/11,
//! long paths, UTF-8 code page) and version info into `opentalk.exe`.
//! The resource script is generated here so the version always follows Cargo.toml.
//!
//! It also makes `opengl32.dll` a delay-loaded import (see [`delay_load_opengl`]), so the daemon,
//! which never draws with OpenGL, does not load it at all.
use std::path::{Path, PathBuf};
use std::process::Command;

/// The WGL entry points `glutin_wgl_sys` links statically (`#[link(name = "opengl32")]`).
const WGL_EXPORTS: &[&str] = &[
    "wglCopyContext",
    "wglCreateContext",
    "wglCreateLayerContext",
    "wglDeleteContext",
    "wglDescribeLayerPlane",
    "wglGetCurrentContext",
    "wglGetCurrentDC",
    "wglGetLayerPaletteEntries",
    "wglGetProcAddress",
    "wglMakeCurrent",
    "wglRealizeLayerPalette",
    "wglSetLayerPaletteEntries",
    "wglShareLists",
    "wglSwapLayerBuffers",
    "wglUseFontBitmapsA",
    "wglUseFontBitmapsW",
    "wglUseFontOutlinesA",
    "wglUseFontOutlinesW",
];

/// Only the Settings window (`--settings`, egui on glow) uses OpenGL, yet a normal import loads
/// `opengl32.dll` into every process start. In the daemon that costs working set and, worse, makes
/// overlay injectors that hook GL processes (RivaTuner/MSI Afterburner, game overlays) treat the
/// daemon as a 3D app: with RTSS running, the daemon's private commit went from ~2 MB to ~58 MB at
/// the first STA `CoInitializeEx`. A delay-load import library (dlltool `--output-delaylib`,
/// resolved by mingw-w64's `__delayLoadHelper2`) is found before the system `libopengl32.a`
/// through the link search path, so the DLL is only loaded on the first WGL call.
/// Best effort: without dlltool the normal import is used.
fn delay_load_opengl(out: &Path) {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("gnu") || std::env::var_os("OPENTALK_NO_DELAYLOAD").is_some() {
        return;
    }
    let dir = out.join("delayload");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let def = dir.join("opengl32.def");
    let body = format!("LIBRARY opengl32.dll\nEXPORTS\n{}\n", WGL_EXPORTS.join("\n"));
    if std::fs::write(&def, body).is_err() {
        return;
    }
    let lib = dir.join("libopengl32.a");
    let ok = Command::new("dlltool")
        .arg("--input-def")
        .arg(&def)
        .args(["--dllname", "opengl32.dll", "--output-delaylib"])
        .arg(&lib)
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        println!("cargo:rustc-link-search=native={}", dir.display());
    } else {
        println!("cargo:warning=dlltool unavailable: opengl32.dll stays a load-time import");
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=OPENTALK_NO_DELAYLOAD");
    println!("cargo:rerun-if-changed=assets/opentalk.ico");
    println!("cargo:rerun-if-changed=assets/opentalk.manifest");
    println!("cargo:rerun-if-env-changed=OPENTALK_NO_RESOURCES");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    delay_load_opengl(&out);
    // OPENTALK_NO_RESOURCES=1 skips embedding (needed once to bootstrap `cargo run --example make_icon`
    // before assets/opentalk.ico exists, or when windres is unavailable).
    if std::env::var_os("OPENTALK_NO_RESOURCES").is_some() {
        return;
    }
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
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
