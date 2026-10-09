//! API keys encrypted at rest with DPAPI (`CryptProtectData`, current-user scope),
//! stored base64 in `secrets.json` as `{ "<name>": "<b64 ciphertext>" }`.
//! Never log values.
//!
//! Well-known names: see `SecretName` — the UI and providers use these constants.
use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::Mutex;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::core::w;

use super::paths;

pub struct SecretName;
impl SecretName {
    pub const OPENAI: &'static str = "openai";
    pub const GROQ: &'static str = "groq";
    pub const XAI: &'static str = "xai";
    pub const MISTRAL: &'static str = "mistral";
    pub const GEMINI: &'static str = "gemini";
    pub const DEEPGRAM: &'static str = "deepgram";
    pub const ASSEMBLYAI: &'static str = "assemblyai";
    pub const TINFOIL: &'static str = "tinfoil";
    pub const OPENROUTER: &'static str = "openrouter";
    pub const ANTHROPIC: &'static str = "anthropic";
    pub const CUSTOM_STT: &'static str = "custom_stt";
    pub const CUSTOM_LLM: &'static str = "custom_llm";
    pub const CORTI_CLIENT_ID: &'static str = "corti_client_id";
    pub const CORTI_CLIENT_SECRET: &'static str = "corti_client_secret";
    pub const AZURE_OPENAI: &'static str = "azure_openai";
    pub const AWS_ACCESS_KEY_ID: &'static str = "aws_access_key_id";
    pub const AWS_SECRET_ACCESS_KEY: &'static str = "aws_secret_access_key";
    pub const VERTEX_SERVICE_ACCOUNT_JSON: &'static str = "vertex_service_account_json";
}

/// Extra DPAPI entropy: a blob copied from another app's file can't be opened by a plain
/// `CryptUnprotectData` call without it.
const ENTROPY: &[u8] = b"OpenTalk";

/// Serializes read-modify-write cycles on `secrets.json` within this process.
static FILE_LOCK: Mutex<()> = Mutex::new(());

type Store = BTreeMap<String, String>;

/// Read and decrypt a secret. `None` if absent or undecryptable.
/// Env override for tests: `OPENTALK_SECRET_<NAME_UPPER>` (e.g. OPENTALK_SECRET_OPENROUTER).
pub fn get(name: &str) -> Option<String> {
    get_with(&paths::secrets_file(), name, &|k| std::env::var(k).ok())
}

/// Encrypt and store (empty string deletes). Surrounding whitespace is trimmed first: a pasted
/// key often carries a trailing newline, which would make every request's auth header invalid.
pub fn set(name: &str, value: &str) -> io::Result<()> {
    set_in(&paths::secrets_file(), name, value)
}

/// Names that currently have a stored value.
pub fn list() -> Vec<String> {
    list_in(&paths::secrets_file())
}

/// "sk-...abcd" style mask for display.
pub fn mask(value: &str) -> String {
    let v = value.trim();
    let n = v.chars().count();
    if n <= 8 {
        return "•".repeat(n);
    }
    let head: String = v.chars().take(3).collect();
    let tail: String = v.chars().skip(n - 4).collect();
    format!("{head}...{tail}")
}

/// Name of the environment variable that overrides secret `name`.
pub fn env_var_name(name: &str) -> String {
    let mut s = String::from("OPENTALK_SECRET_");
    s.extend(name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }));
    s
}

fn get_with(path: &Path, name: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    if let Some(v) = env(&env_var_name(name)) {
        let v = v.trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let store = read_store(path)?;
    let blob = B64.decode(store.get(name)?).ok()?;
    let plain = unprotect(&blob).ok()?;
    String::from_utf8(plain).ok()
}

fn set_in(path: &Path, name: &str, value: &str) -> io::Result<()> {
    let value = value.trim();
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Store>(&bytes) {
            Ok(s) => s,
            Err(_) => {
                // Unreadable file: keep it for inspection and start fresh.
                let _ = std::fs::rename(path, path.with_extension("json.bad"));
                Store::new()
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Store::new(),
        Err(e) => return Err(e),
    };
    if value.is_empty() {
        if store.remove(name).is_none() {
            return Ok(());
        }
    } else {
        store.insert(name.to_string(), B64.encode(protect(value.as_bytes())?));
    }
    write_atomic(path, &serde_json::to_vec_pretty(&store).map_err(io::Error::other)?)
}

fn list_in(path: &Path) -> Vec<String> {
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    read_store(path).map(|s| s.into_keys().collect()).unwrap_or_default()
}

fn read_store(path: &Path) -> Option<Store> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Write temp file in the same directory, then rename over the target.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
}

/// Copy a DPAPI-allocated output blob into a `Vec`, zeroing and freeing the original.
///
/// SAFETY: `out` must have been filled in by a successful `CryptProtectData` /
/// `CryptUnprotectData` call, so `pbData` is a `LocalAlloc` block of `cbData` bytes.
unsafe fn take_blob(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    if out.pbData.is_null() {
        return Vec::new();
    }
    unsafe {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
        let _ = LocalFree(Some(HLOCAL(out.pbData as *mut _)));
        v
    }
}

/// Encrypt for the current Windows user.
pub fn protect(data: &[u8]) -> io::Result<Vec<u8>> {
    let input = blob(data);
    let entropy = blob(ENTROPY);
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input`/`entropy` point at live slices for the call; `out` receives a
    // LocalAlloc'd buffer that `take_blob` frees.
    unsafe {
        CryptProtectData(&input, w!("OpenTalk"), Some(&entropy), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            .map_err(|e| io::Error::other(format!("CryptProtectData: {e}")))?;
        Ok(take_blob(out))
    }
}

/// Decrypt a blob produced by [`protect`] under the same Windows user.
pub fn unprotect(data: &[u8]) -> io::Result<Vec<u8>> {
    let input = blob(data);
    let entropy = blob(ENTROPY);
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as in `protect`.
    unsafe {
        CryptUnprotectData(&input, None, Some(&entropy), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            .map_err(|e| io::Error::other(format!("CryptUnprotectData: {e}")))?;
        Ok(take_blob(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_file(tag: &str) -> std::path::PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "opentalk-secrets-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d.join("secrets.json")
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn dpapi_round_trip() {
        let enc = protect(b"sk-test-123").unwrap();
        assert_ne!(enc, b"sk-test-123");
        assert_eq!(unprotect(&enc).unwrap(), b"sk-test-123");
        // Garbage must fail, not panic.
        assert!(unprotect(b"definitely not a dpapi blob").is_err());
    }

    #[test]
    fn set_get_list_delete() {
        let p = temp_file("sgld");
        assert_eq!(get_with(&p, "openai", &no_env), None);
        set_in(&p, "openai", "sk-abc").unwrap();
        set_in(&p, "groq", "gsk-xyz").unwrap();
        assert_eq!(get_with(&p, "openai", &no_env).as_deref(), Some("sk-abc"));
        assert_eq!(list_in(&p), vec!["groq".to_string(), "openai".to_string()]);
        // The plaintext never touches disk.
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(!raw.contains("sk-abc") && !raw.contains("gsk-xyz"));
        // Overwrite, then delete with "".
        set_in(&p, "openai", "sk-new").unwrap();
        assert_eq!(get_with(&p, "openai", &no_env).as_deref(), Some("sk-new"));
        set_in(&p, "openai", "").unwrap();
        assert_eq!(get_with(&p, "openai", &no_env), None);
        assert_eq!(list_in(&p), vec!["groq".to_string()]);
        // Deleting something absent is a no-op.
        set_in(&p, "nothing", "").unwrap();
        // Pasted keys are trimmed; whitespace-only deletes.
        set_in(&p, "openai", "  sk-pasted
").unwrap();
        assert_eq!(get_with(&p, "openai", &no_env).as_deref(), Some("sk-pasted"));
        set_in(&p, "openai", " 
").unwrap();
        assert_eq!(get_with(&p, "openai", &no_env), None);
    }

    #[test]
    fn unicode_and_long_values() {
        let p = temp_file("uni");
        let v = format!("é✓-{}", "x".repeat(10_000));
        set_in(&p, "custom_llm", &v).unwrap();
        assert_eq!(get_with(&p, "custom_llm", &no_env).as_deref(), Some(v.as_str()));
    }

    #[test]
    fn env_override_wins_and_ignores_blank() {
        let p = temp_file("env");
        set_in(&p, "openrouter", "stored").unwrap();
        let env = |k: &str| (k == "OPENTALK_SECRET_OPENROUTER").then(|| " from-env ".to_string());
        assert_eq!(get_with(&p, "openrouter", &env).as_deref(), Some("from-env"));
        let blank = |k: &str| (k == "OPENTALK_SECRET_OPENROUTER").then(|| "  ".to_string());
        assert_eq!(get_with(&p, "openrouter", &blank).as_deref(), Some("stored"));
        // Env works with no file at all.
        let q = temp_file("env2");
        assert_eq!(get_with(&q, "openrouter", &env).as_deref(), Some("from-env"));
    }

    #[test]
    fn env_var_names() {
        assert_eq!(env_var_name("openrouter"), "OPENTALK_SECRET_OPENROUTER");
        assert_eq!(env_var_name("aws_access_key_id"), "OPENTALK_SECRET_AWS_ACCESS_KEY_ID");
        assert_eq!(env_var_name("a-b.c"), "OPENTALK_SECRET_A_B_C");
    }

    #[test]
    fn corrupt_store_is_tolerated() {
        let p = temp_file("bad");
        std::fs::write(&p, b"{not json").unwrap();
        assert_eq!(get_with(&p, "openai", &no_env), None);
        assert!(list_in(&p).is_empty());
        set_in(&p, "openai", "sk-1").unwrap();
        assert_eq!(get_with(&p, "openai", &no_env).as_deref(), Some("sk-1"));
        assert!(p.with_extension("json.bad").exists());
        // Undecryptable entries yield None without poisoning the others.
        std::fs::write(&p, br#"{"openai":"!!!notbase64","groq":"AAAA"}"#).unwrap();
        assert_eq!(get_with(&p, "openai", &no_env), None);
        assert_eq!(get_with(&p, "groq", &no_env), None);
    }

    #[test]
    fn no_tmp_files_left_behind() {
        let p = temp_file("tmp");
        set_in(&p, "openai", "a").unwrap();
        set_in(&p, "openai", "b").unwrap();
        let n = std::fs::read_dir(p.parent().unwrap()).unwrap().count();
        assert_eq!(n, 1);
    }

    #[test]
    fn mask_variants() {
        assert_eq!(mask("sk-1234567890abcd"), "sk-...abcd");
        assert_eq!(mask("  sk-1234567890abcd \n"), "sk-...abcd");
        assert_eq!(mask("short"), "•••••");
        assert_eq!(mask("12345678"), "••••••••");
        assert_eq!(mask("123456789"), "123...6789");
        assert_eq!(mask(""), "");
        // Multi-byte characters must not split a code point.
        assert_eq!(mask("ééééééééééé"), "ééé...éééé");
    }
}
