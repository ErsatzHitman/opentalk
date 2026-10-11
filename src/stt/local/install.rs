//! Install the sherpa-onnx runtime and the local models under `%APPDATA%\OpenTalk\local`.
//!
//! Layout:
//! ```text
//! local\runtime-1.13.8\   sherpa-onnx-{offline,online}-websocket-server.exe, ot-onnxrt.dll, runtime.json
//! local\models\<id>\      the model files and manifest.json (which file plays which role)
//! local\work\             downloads and unpacking in progress (cleared on every install)
//! ```
//! The runtime comes from two pinned, hash-checked packages of the same sherpa-onnx release: the
//! PyPI wheel `sherpa-onnx-bin` (the Windows executables, C runtime linked statically) and the npm
//! package `sherpa-onnx-win-x64` (its `onnxruntime.dll`). Model archives are unpacked with the
//! `tar.exe` that Windows 10 1803+ ships (libarchive: zip, gzip and bzip2). A model folder only
//! appears once it is complete, so a cancelled or failed install leaves nothing half-done behind.
use super::catalog::{Kind, LocalModel, Source};
use super::pe;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub const RUNTIME_VERSION: &str = "1.13.8";
pub const OFFLINE_SERVER: &str = "sherpa-onnx-offline-websocket-server.exe";
pub const ONLINE_SERVER: &str = "sherpa-onnx-online-websocket-server.exe";
/// onnxruntime.dll under a private name (see `pe`): never shadowed by System32's copy.
pub const ORT_PRIVATE_NAME: &str = "ot-onnxrt.dll";

struct Artifact {
    url: &'static str,
    sha256: &'static str,
    /// Members to extract (paths inside the package) and the name each gets in the runtime folder.
    members: &'static [(&'static str, &'static str)],
}

const WHEEL: Artifact = Artifact {
    url: "https://files.pythonhosted.org/packages/38/b9/ad8cd468f0adb37f2701c5067d5b19b141bf815c6434a8749b02d688f3b9/sherpa_onnx_bin-1.13.8-py3-none-win_amd64.whl",
    sha256: "9549d3c1a9ec325fb7315006407ffde5bdeee99f3625c03d0b7967009e869a9a",
    members: &[
        ("sherpa_onnx_bin-1.13.8.data/data/Scripts/sherpa-onnx-offline-websocket-server.exe", OFFLINE_SERVER),
        ("sherpa_onnx_bin-1.13.8.data/data/Scripts/sherpa-onnx-online-websocket-server.exe", ONLINE_SERVER),
    ],
};

const ORT: Artifact = Artifact {
    url: "https://registry.npmjs.org/sherpa-onnx-win-x64/-/sherpa-onnx-win-x64-1.13.8.tgz",
    sha256: "fe522f02a5c113c2567a43982107ef41ae517f9a431931e90731e7e4d4341073",
    members: &[("package/onnxruntime.dll", ORT_PRIVATE_NAME)],
};

/// Approximate size of the runtime download (both packages), for the confirmation text.
pub const RUNTIME_DOWNLOAD_MB: u32 = 27;

/// `OPENTALK_LOCAL_DIR` (tests), else `%APPDATA%\OpenTalk\local`.
pub fn root() -> PathBuf {
    match std::env::var_os("OPENTALK_LOCAL_DIR").filter(|v| !v.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => crate::core::paths::app_dir().join("local"),
    }
}

pub fn runtime_dir() -> PathBuf {
    root().join(format!("runtime-{RUNTIME_VERSION}"))
}

pub fn models_dir() -> PathBuf {
    root().join("models")
}

pub fn model_dir(id: &str) -> PathBuf {
    models_dir().join(id)
}

fn work_dir() -> PathBuf {
    root().join("work")
}

/// The runtime is unpacked, patched and marked complete.
pub fn runtime_ready() -> bool {
    let d = runtime_dir();
    d.join("runtime.json").is_file() && [OFFLINE_SERVER, ONLINE_SERVER, ORT_PRIVATE_NAME].iter().all(|f| d.join(f).is_file())
}

/// Which file of an installed model plays which role (paths relative to the model folder).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    pub id: String,
    pub files: BTreeMap<String, String>,
}

impl Manifest {
    pub fn path(&self, dir: &Path, role: &str) -> Option<PathBuf> {
        self.files.get(role).map(|rel| dir.join(rel))
    }
}

/// The manifest of an installed model, `None` when it is not (completely) installed.
pub fn installed(id: &str) -> Option<Manifest> {
    let dir = model_dir(id);
    let m: Manifest = serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).ok()?).ok()?;
    (m.id == id && m.files.values().all(|rel| dir.join(rel).exists())).then_some(m)
}

/// Disk space an installed model takes (MB).
pub fn installed_mb(id: &str) -> u64 {
    fn walk(p: &Path) -> u64 {
        std::fs::read_dir(p).map_or(0, |rd| {
            rd.flatten().map(|e| match e.file_type() {
                Ok(t) if t.is_dir() => walk(&e.path()),
                _ => e.metadata().map_or(0, |m| m.len()),
            }).sum()
        })
    }
    walk(&model_dir(id)) / 1_000_000
}

/// Delete an installed model.
pub fn remove(id: &str) -> std::io::Result<()> {
    let dir = model_dir(id);
    if dir.exists() { std::fs::remove_dir_all(dir) } else { Ok(()) }
}

/// What an install is doing, for progress lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Runtime,
    Download,
    Unpack,
}

/// Progress callback (`done`, `total` bytes of the current stage) and the cancel flag.
pub struct Progress<'a> {
    pub on: &'a (dyn Fn(Stage, u64, Option<u64>) + Sync),
    pub cancel: &'a AtomicBool,
}

impl Progress<'_> {
    fn check(&self) -> Result<(), String> {
        if self.cancel.load(Ordering::Relaxed) { Err(CANCELLED.into()) } else { Ok(()) }
    }
}

pub const CANCELLED: &str = "cancelled";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Stream `url` into `dest` (via `dest.part`), checking `sha256` when given.
fn download(url: &str, dest: &Path, sha256: Option<&str>, stage: Stage, p: &Progress) -> Result<u64, String> {
    p.check()?;
    let url = &mirrored(url);
    let dl = crate::core::http::open_download(url).map_err(|e| format!("cannot reach {}: {e:?}", host(url)))?;
    if !(200..300).contains(&dl.status) {
        return Err(format!("{} answered HTTP {}", host(url), dl.status));
    }
    let part = dest.with_extension("part");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&part).map_err(|e| format!("cannot write {}: {e}", part.display()))?);
    let (mut body, total) = (dl.body, dl.len);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    let mut last_report = std::time::Instant::now();
    let result = loop {
        if let Err(e) = p.check() {
            break Err(e);
        }
        match body.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                hasher.update(&buf[..n]);
                if let Err(e) = out.write_all(&buf[..n]) {
                    break Err(format!("cannot write the download (disk full?): {e}"));
                }
                done += n as u64;
                if last_report.elapsed().as_millis() >= 100 {
                    (p.on)(stage, done, total);
                    last_report = std::time::Instant::now();
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => break Err(format!("the download from {} broke off: {e}", host(url))),
        }
    };
    let flushed = out.flush().map_err(|e| format!("cannot write the download: {e}"));
    drop(out);
    if let Err(e) = result.and(flushed) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    if total.is_some_and(|t| t != done) {
        let _ = std::fs::remove_file(&part);
        return Err(format!("the download from {} was cut short", host(url)));
    }
    if let Some(want) = sha256 {
        let got = hex(&hasher.finalize());
        if !got.eq_ignore_ascii_case(want) {
            let _ = std::fs::remove_file(&part);
            return Err(format!("the file from {} failed its integrity check", host(url)));
        }
    }
    (p.on)(stage, done, total.or(Some(done)));
    std::fs::rename(&part, dest).map_err(|e| format!("cannot save {}: {e}", dest.display()))?;
    Ok(done)
}

/// `OPENTALK_LOCAL_MIRROR` (an `http(s)://` folder holding the same file names) replaces the
/// download hosts, for offline or managed installs; the runtime's hashes are still checked.
fn mirrored(url: &str) -> String {
    match std::env::var("OPENTALK_LOCAL_MIRROR").ok().map(|m| m.trim().trim_end_matches('/').to_string()).filter(|m| !m.is_empty()) {
        Some(m) => format!("{m}/{}", url.rsplit('/').next().unwrap_or(url)),
        None => url.to_string(),
    }
}

fn host(url: &str) -> &str {
    url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or(url)
}

/// Windows' own bsdtar (`OPENTALK_TAR` overrides it for tests).
fn tar_exe() -> PathBuf {
    if let Some(t) = std::env::var_os("OPENTALK_TAR").filter(|v| !v.is_empty()) {
        return PathBuf::from(t);
    }
    let sys = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    sys.join("System32").join("tar.exe")
}

/// Unpack `archive` (zip, .tgz or .tar.bz2) into `into`; only `members` when given.
fn extract(archive: &Path, into: &Path, members: &[&str]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::fs::create_dir_all(into).map_err(|e| format!("cannot create {}: {e}", into.display()))?;
    let tar = tar_exe();
    // stderr goes to a file next to the archive (no pipe: simpler, and it works under Wine too)
    let log = archive.with_extension("tar-log");
    let err_file = std::fs::File::create(&log).map_err(|e| format!("cannot write {}: {e}", log.display()))?;
    let status = std::process::Command::new(&tar)
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .args(members)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(err_file)
        .status()
        .map_err(|e| format!("cannot run {} to unpack the model: {e}", tar.display()))?;
    let err = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    if !status.success() {
        return Err(format!("unpacking failed: {}", err.trim().lines().last().unwrap_or("tar exited with an error")));
    }
    Ok(())
}

fn fresh_dir(p: &Path) -> Result<(), String> {
    if p.exists() {
        std::fs::remove_dir_all(p).map_err(|e| format!("cannot clear {}: {e}", p.display()))?;
    }
    std::fs::create_dir_all(p).map_err(|e| format!("cannot create {}: {e}", p.display()))
}

/// Download, verify, unpack and patch the runtime (no-op when already there).
pub fn install_runtime(p: &Progress) -> Result<(), String> {
    if runtime_ready() {
        return Ok(());
    }
    let work = work_dir().join("runtime");
    fresh_dir(&work)?;
    let staged = work.join("out");
    std::fs::create_dir_all(&staged).map_err(|e| e.to_string())?;
    for (i, a) in [&WHEEL, &ORT].into_iter().enumerate() {
        let file = work.join(format!("pkg{i}"));
        download(a.url, &file, Some(a.sha256), Stage::Runtime, p)?;
        let unpacked = work.join(format!("x{i}"));
        let names: Vec<&str> = a.members.iter().map(|m| m.0).collect();
        extract(&file, &unpacked, &names)?;
        for (member, name) in a.members {
            let src = unpacked.join(member);
            std::fs::rename(&src, staged.join(name)).map_err(|e| format!("the runtime package is missing {member}: {e}"))?;
        }
    }
    for exe in [OFFLINE_SERVER, ONLINE_SERVER] {
        patch_imports(&staged.join(exe))?;
    }
    std::fs::write(staged.join("runtime.json"), format!("{{\"version\":\"{RUNTIME_VERSION}\",\"onnxruntime\":\"{ORT_PRIVATE_NAME}\"}}"))
        .map_err(|e| e.to_string())?;
    let dest = runtime_dir();
    if dest.exists() {
        std::fs::remove_dir_all(&dest).map_err(|e| format!("cannot replace {}: {e}", dest.display()))?;
    }
    std::fs::rename(&staged, &dest).map_err(|e| format!("cannot finish the runtime install: {e}"))?;
    let _ = std::fs::remove_dir_all(&work);
    Ok(())
}

/// Point an executable at the privately named onnxruntime (see `pe`).
fn patch_imports(exe: &Path) -> Result<(), String> {
    let mut img = std::fs::read(exe).map_err(|e| format!("cannot read {}: {e}", exe.display()))?;
    let n = pe::rename_import(&mut img, "onnxruntime.dll", ORT_PRIVATE_NAME).map_err(|e| format!("{}: {e}", exe.display()))?;
    if n == 0 {
        return Err(format!("{} does not import onnxruntime.dll", exe.display()));
    }
    std::fs::write(exe, img).map_err(|e| format!("cannot write {}: {e}", exe.display()))
}

/// Download and unpack `m`, resolve its files and keep only those. Returns its manifest.
pub fn install_model(m: &LocalModel, p: &Progress) -> Result<Manifest, String> {
    let work = work_dir().join(m.id);
    fresh_dir(&work)?;
    let unpacked = work.join("x");
    let model_root = match m.source {
        Source::Release { archive, dir } => {
            let file = work.join(archive);
            download(&m.source.urls(), &file, None, Stage::Download, p)?;
            p.check()?;
            (p.on)(Stage::Unpack, 0, None);
            extract(&file, &unpacked, &[])?;
            let _ = std::fs::remove_file(&file);
            let inner = unpacked.join(dir);
            if inner.is_dir() { inner } else { unpacked.clone() }
        }
        Source::HuggingFace { repo, revision } => {
            download_repo(repo, revision, &unpacked, p)?;
            unpacked.clone()
        }
    };
    p.check()?;
    let files = resolve(m.kind, &model_root)?;
    prune(&model_root, &files);
    let manifest = Manifest { id: m.id.to_string(), files };
    std::fs::write(model_root.join("manifest.json"), serde_json::to_vec_pretty(&manifest).expect("manifest serializes")).map_err(|e| e.to_string())?;
    let dest = model_dir(m.id);
    std::fs::create_dir_all(models_dir()).map_err(|e| e.to_string())?;
    if dest.exists() {
        std::fs::remove_dir_all(&dest).map_err(|e| format!("cannot replace {}: {e}", dest.display()))?;
    }
    std::fs::rename(&model_root, &dest).map_err(|e| format!("cannot finish the install: {e}"))?;
    let _ = std::fs::remove_dir_all(&work);
    Ok(manifest)
}

/// Files of a Hugging Face repo that a sherpa-onnx model needs (no samples, docs or git files).
fn wanted_repo_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    !(lower.starts_with("test_wavs/")
        || lower.contains("/test_wavs/")
        || name.starts_with("readme")
        || name.starts_with('.')
        || name.ends_with(".wav")
        || name.ends_with(".md")
        || name.ends_with(".py")
        || name.ends_with(".ipynb")
        || name == "license")
}

fn download_repo(repo: &str, revision: &str, into: &Path, p: &Progress) -> Result<(), String> {
    let list_url = format!("https://huggingface.co/api/models/{repo}/tree/{revision}?recursive=true");
    let resp = crate::core::http::get(&list_url, &[], std::time::Duration::from_secs(30)).map_err(|e| format!("cannot reach huggingface.co: {e:?}"))?;
    if !resp.ok() {
        return Err(format!("huggingface.co answered HTTP {} for {repo}", resp.status));
    }
    let entries: Vec<serde_json::Value> = serde_json::from_str(&resp.body).map_err(|_| "huggingface.co sent an unexpected file list".to_string())?;
    let files: Vec<(String, u64)> = entries
        .iter()
        .filter(|e| e["type"] == "file")
        .filter_map(|e| Some((e["path"].as_str()?.to_string(), e["size"].as_u64().unwrap_or(0))))
        .filter(|(path, _)| wanted_repo_file(path) && !path.contains(".."))
        .collect();
    if files.is_empty() {
        return Err(format!("{repo} has no model files"));
    }
    let total: u64 = files.iter().map(|f| f.1).sum();
    let mut before = 0u64;
    for (path, size) in &files {
        let dest = into.join(path.replace('/', "\\"));
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let url = format!("https://huggingface.co/{repo}/resolve/{revision}/{path}");
        let offset = before;
        let on = move |stage: Stage, done: u64, _t: Option<u64>| (p.on)(stage, offset + done, Some(total));
        download(&url, &dest, None, Stage::Download, &Progress { on: &on, cancel: p.cancel })?;
        before += size;
    }
    Ok(())
}

/// Every file under `dir`, as `/`-separated relative paths, sorted.
fn list_files(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, p: &Path, out: &mut Vec<String>, depth: u32) {
        let Ok(rd) = std::fs::read_dir(p) else { return };
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                if depth < 4 {
                    walk(base, &path, out, depth + 1);
                }
            } else if let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut v = Vec::new();
    walk(dir, dir, &mut v, 0);
    v.sort();
    v
}

/// The roles `kind` needs, each with the file names that may play it, best first. A name that
/// starts with `-` matches as a suffix (Whisper files carry the model name: `turbo-encoder.onnx`).
fn roles(kind: Kind) -> &'static [(&'static str, &'static [&'static str])] {
    match kind {
        Kind::Whisper => &[
            ("encoder", &["-encoder.int8.onnx", "-encoder.onnx"]),
            ("decoder", &["-decoder.int8.onnx", "-decoder.onnx"]),
            ("tokens", &["-tokens.txt"]),
        ],
        Kind::Moonshine => &[
            ("preprocessor", &["preprocess.onnx"]),
            ("encoder", &["encode.int8.onnx", "encode.onnx"]),
            ("uncached_decoder", &["uncached_decode.int8.onnx", "uncached_decode.onnx"]),
            ("cached_decoder", &["cached_decode.int8.onnx", "cached_decode.onnx"]),
            ("tokens", &["tokens.txt"]),
        ],
        Kind::NemoTransducer | Kind::StreamingTransducer => &[
            ("encoder", &["encoder.int8.onnx", "encoder.onnx"]),
            ("decoder", &["decoder.int8.onnx", "decoder.onnx"]),
            ("joiner", &["joiner.int8.onnx", "joiner.onnx"]),
            ("tokens", &["tokens.txt"]),
        ],
        #[cfg(test)]
        Kind::TestCtc => &[("model", &["model.onnx"]), ("tokens", &["tokens.txt"])],
        Kind::Qwen3Asr => &[
            ("conv_frontend", &["conv_frontend.int8.onnx", "conv_frontend.onnx"]),
            ("encoder", &["encoder.int8.onnx", "encoder.onnx"]),
            ("decoder", &["decoder.int8.onnx", "decoder.onnx"]),
            // the tokenizer is a folder: the one holding vocab.json
            ("tokenizer", &["vocab.json"]),
        ],
    }
}

/// Map each role of `kind` to a file of `dir` (see [`roles`]).
pub fn resolve(kind: Kind, dir: &Path) -> Result<BTreeMap<String, String>, String> {
    let files = list_files(dir);
    let name_of = |rel: &str| rel.rsplit('/').next().unwrap_or(rel).to_string();
    let mut out = BTreeMap::new();
    for (role, wanted) in roles(kind) {
        let hit = wanted.iter().find_map(|w| {
            files.iter().find(|rel| {
                let n = name_of(rel);
                if let Some(suffix) = w.strip_prefix('-') { n.ends_with(&format!("-{suffix}")) } else { n == *w }
            })
        });
        let Some(rel) = hit else { return Err(format!("the model is missing its {role} file")) };
        let rel = if *role == "tokenizer" { rel.rsplit_once('/').map_or(".".to_string(), |(d, _)| d.to_string()) } else { rel.clone() };
        out.insert(role.to_string(), rel);
    }
    Ok(out)
}

/// Delete what the model does not use: other precisions of the network files, samples, docs.
fn prune(dir: &Path, keep: &BTreeMap<String, String>) {
    let kept: Vec<&String> = keep.values().collect();
    for rel in list_files(dir) {
        let n = rel.to_ascii_lowercase();
        let in_kept_dir = kept.iter().any(|k| rel.starts_with(&format!("{k}/")));
        let is_kept = kept.iter().any(|k| rel == **k || (rel.starts_with(k.as_str()) && n.contains(".onnx")));
        let network = n.ends_with(".onnx") || n.ends_with(".onnx.data") || n.ends_with(".weights") || n.ends_with(".ort");
        let sample = n.contains("test_wavs/") || n.ends_with(".wav");
        if (network && !is_kept && !in_kept_dir) || sample {
            let _ = std::fs::remove_file(dir.join(&rel));
        }
    }
    let _ = std::fs::remove_dir_all(dir.join("test_wavs"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[&str]) -> PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("ot-local-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        for f in files {
            let p = dir.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"x").unwrap();
        }
        dir
    }

    #[test]
    fn whisper_files_resolve_by_suffix_and_prefer_int8() {
        let dir = tree(&["turbo-encoder.onnx", "turbo-encoder.int8.onnx", "turbo-decoder.onnx", "turbo-decoder.int8.onnx", "turbo-tokens.txt", "test_wavs/0.wav", "README.md"]);
        let f = resolve(Kind::Whisper, &dir).unwrap();
        assert_eq!(f["encoder"], "turbo-encoder.int8.onnx");
        assert_eq!(f["decoder"], "turbo-decoder.int8.onnx");
        assert_eq!(f["tokens"], "turbo-tokens.txt");
        prune(&dir, &f);
        let left = list_files(&dir);
        assert_eq!(left, vec!["README.md", "turbo-decoder.int8.onnx", "turbo-encoder.int8.onnx", "turbo-tokens.txt"], "fp32 copies and samples are gone");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn transducer_moonshine_and_qwen_layouts() {
        let dir = tree(&["encoder.int8.onnx", "decoder.int8.onnx", "joiner.int8.onnx", "tokens.txt"]);
        let f = resolve(Kind::NemoTransducer, &dir).unwrap();
        assert_eq!((f["encoder"].as_str(), f["joiner"].as_str(), f["tokens"].as_str()), ("encoder.int8.onnx", "joiner.int8.onnx", "tokens.txt"));
        let _ = std::fs::remove_dir_all(dir);

        let dir = tree(&["preprocess.onnx", "encode.int8.onnx", "uncached_decode.int8.onnx", "cached_decode.int8.onnx", "tokens.txt"]);
        assert_eq!(resolve(Kind::Moonshine, &dir).unwrap().len(), 5);
        let _ = std::fs::remove_dir_all(dir);

        // the 1.7B community export keeps the decoder weights in an external .data file
        let dir = tree(&["conv_frontend.onnx", "encoder.int8.onnx", "decoder.int8.onnx", "decoder.int8.onnx.data", "tokenizer/vocab.json", "tokenizer/merges.txt"]);
        let f = resolve(Kind::Qwen3Asr, &dir).unwrap();
        assert_eq!(f["tokenizer"], "tokenizer", "the tokenizer role is the folder");
        prune(&dir, &f);
        let left = list_files(&dir);
        assert!(left.contains(&"decoder.int8.onnx.data".to_string()), "external weights stay with their graph: {left:?}");
        assert!(left.contains(&"tokenizer/merges.txt".to_string()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_role_is_an_error_naming_it() {
        let dir = tree(&["encoder.int8.onnx", "decoder.int8.onnx", "tokens.txt"]);
        assert_eq!(resolve(Kind::NemoTransducer, &dir).unwrap_err(), "the model is missing its joiner file");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn repo_files_skip_samples_and_docs() {
        for keep in ["encoder.int8.onnx", "decoder.int8.onnx.data", "tokenizer/vocab.json", "tokenizer/merges.txt", "tokenizer/tokenizer_config.json"] {
            assert!(wanted_repo_file(keep), "{keep}");
        }
        for skip in ["README.md", ".gitattributes", "test_wavs/en.wav", "export.py", "LICENSE", "sub/test_wavs/x.txt"] {
            assert!(!wanted_repo_file(skip), "{skip}");
        }
    }

    #[test]
    fn installed_needs_a_complete_manifest() {
        let root = std::env::temp_dir().join(format!("ot-local-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // SAFETY: tests touching OPENTALK_LOCAL_DIR live in this module and set the same value.
        unsafe { std::env::set_var("OPENTALK_LOCAL_DIR", &root) };
        assert_eq!(installed("m"), None);
        let dir = model_dir("m");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tokens.txt"), b"x").unwrap();
        let m = Manifest { id: "m".into(), files: BTreeMap::from([("tokens".to_string(), "tokens.txt".to_string()), ("encoder".to_string(), "e.onnx".to_string())]) };
        std::fs::write(dir.join("manifest.json"), serde_json::to_vec(&m).unwrap()).unwrap();
        assert_eq!(installed("m"), None, "a listed file is missing");
        std::fs::write(dir.join("e.onnx"), b"x").unwrap();
        assert_eq!(installed("m"), Some(m));
        remove("m").unwrap();
        assert_eq!(installed("m"), None);
        assert!(!runtime_ready());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_mirror_keeps_the_file_name() {
        assert_eq!(mirrored("https://example.org/a/b/model.tar.bz2"), "https://example.org/a/b/model.tar.bz2");
    }

    #[test]
    fn hosts_and_hex() {
        assert_eq!(host("https://files.pythonhosted.org/packages/x.whl"), "files.pythonhosted.org");
        assert_eq!(hex(&[0, 15, 255]), "000fff");
    }
}
