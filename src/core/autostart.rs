//! Launch at login via HKCU\Software\Microsoft\Windows\CurrentVersion\Run\OpenTalk = "<exe>" --hidden.
//! `is_enabled` must also honour `...\Explorer\StartupApproved\Run\OpenTalk` (first byte 0x03 = disabled
//! by Task Manager/Settings), mirroring OpenWhispr's executableWillLaunchAtLogin lesson.
use std::io;

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW,
};
use windows::core::{PCWSTR, HSTRING};

/// Registry value name under both keys.
const VALUE_NAME: &str = "OpenTalk";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APPROVED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

/// Registry locations, overridable so tests can use a scratch key.
struct Keys<'a> {
    run: &'a str,
    approved: &'a str,
}

const REAL: Keys<'static> = Keys { run: RUN_KEY, approved: APPROVED_KEY };

/// True if the Run entry exists and Task Manager / Settings hasn't disabled it.
pub fn is_enabled() -> bool {
    is_enabled_in(&REAL)
}

/// Create or remove the Run entry. Enabling also clears a "disabled" StartupApproved flag so
/// the entry actually launches.
pub fn set_enabled(enabled: bool) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    set_enabled_in(&REAL, enabled, &exe.to_string_lossy())
}

/// `"<exe>" --hidden`: the daemon starts straight into the tray.
pub fn command_line(exe: &str) -> String {
    format!("\"{exe}\" --hidden")
}

/// StartupApproved data: odd first byte (0x03, 0x07) = disabled by the user, 0x02/0x06 = enabled.
fn approved_bytes_disabled(data: &[u8]) -> bool {
    data.first().is_some_and(|b| b & 1 == 1)
}

fn is_enabled_in(keys: &Keys) -> bool {
    let Some((_, run)) = read_value(keys.run, VALUE_NAME) else { return false };
    if run.len() < 2 || run.iter().all(|&b| b == 0) {
        return false;
    }
    match read_value(keys.approved, VALUE_NAME) {
        Some((_, data)) => !approved_bytes_disabled(&data),
        None => true,
    }
}

fn set_enabled_in(keys: &Keys, enabled: bool, exe: &str) -> io::Result<()> {
    if enabled {
        let wide: Vec<u16> = command_line(exe).encode_utf16().chain(std::iter::once(0)).collect();
        write_value(keys.run, VALUE_NAME, REG_SZ, &u16s_to_bytes(&wide))?;
        // Absent = enabled; a disabled flag would silence the entry we just wrote.
        if read_value(keys.approved, VALUE_NAME).is_some_and(|(_, d)| approved_bytes_disabled(&d)) {
            delete_value(keys.approved, VALUE_NAME)?;
        }
    } else {
        delete_value(keys.run, VALUE_NAME)?;
        delete_value(keys.approved, VALUE_NAME)?;
    }
    Ok(())
}

fn u16s_to_bytes(w: &[u16]) -> Vec<u8> {
    w.iter().flat_map(|c| c.to_le_bytes()).collect()
}

fn win_err(op: &str, code: WIN32_ERROR) -> io::Error {
    io::Error::new(io::Error::from_raw_os_error(code.0 as i32).kind(), format!("{op}: error {}", code.0))
}

/// Closes the key on drop.
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful RegOpenKeyExW/RegCreateKeyExW and is closed once.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn open(path: &str, access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Result<Key, WIN32_ERROR> {
    let mut h = HKEY::default();
    // SAFETY: `h` is a valid out pointer; the HSTRING outlives the call.
    let r = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &HSTRING::from(path), None, access, &mut h) };
    if r == ERROR_SUCCESS { Ok(Key(h)) } else { Err(r) }
}

/// Read a value; `None` if the key or value is missing (or unreadable).
fn read_value(key: &str, name: &str) -> Option<(REG_VALUE_TYPE, Vec<u8>)> {
    let k = open(key, KEY_READ).ok()?;
    let name = HSTRING::from(name);
    let mut ty = REG_VALUE_TYPE::default();
    let mut len = 0u32;
    // SAFETY: first call only queries type and size.
    let r = unsafe { RegQueryValueExW(k.0, &name, None, Some(&mut ty), None, Some(&mut len)) };
    if r != ERROR_SUCCESS {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    // SAFETY: `buf` is `len` bytes; `len` is updated to the bytes actually written.
    let r = unsafe { RegQueryValueExW(k.0, &name, None, Some(&mut ty), Some(buf.as_mut_ptr()), Some(&mut len)) };
    if r != ERROR_SUCCESS {
        return None;
    }
    buf.truncate(len as usize);
    Some((ty, buf))
}

fn write_value(key: &str, name: &str, ty: REG_VALUE_TYPE, data: &[u8]) -> io::Result<()> {
    let mut h = HKEY::default();
    // SAFETY: valid out pointer; strings outlive the call.
    let r = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(key),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut h,
            None,
        )
    };
    if r != ERROR_SUCCESS {
        return Err(win_err("RegCreateKeyExW", r));
    }
    let k = Key(h);
    // SAFETY: `data` is a live slice for the call.
    let r = unsafe { RegSetValueExW(k.0, &HSTRING::from(name), None, ty, Some(data)) };
    if r == ERROR_SUCCESS { Ok(()) } else { Err(win_err("RegSetValueExW", r)) }
}

/// Delete a value; a missing key or value counts as success.
fn delete_value(key: &str, name: &str) -> io::Result<()> {
    let k = match open(key, KEY_SET_VALUE) {
        Ok(k) => k,
        Err(e) if e == ERROR_FILE_NOT_FOUND => return Ok(()),
        Err(e) => return Err(win_err("RegOpenKeyExW", e)),
    };
    // SAFETY: valid open key; the HSTRING outlives the call.
    let r = unsafe { RegDeleteValueW(k.0, &HSTRING::from(name)) };
    if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND { Ok(()) } else { Err(win_err("RegDeleteValueW", r)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Registry::{REG_BINARY, RegDeleteKeyW, RegDeleteTreeW};

    #[test]
    fn command_line_quotes_exe() {
        assert_eq!(command_line(r"C:\Program Files\OpenTalk\opentalk.exe"), r#""C:\Program Files\OpenTalk\opentalk.exe" --hidden"#);
    }

    #[test]
    fn approved_flag_semantics() {
        assert!(approved_bytes_disabled(&[0x03, 0, 0, 0]));
        assert!(approved_bytes_disabled(&[0x07, 0, 0, 0]));
        assert!(!approved_bytes_disabled(&[0x02, 0, 0, 0]));
        assert!(!approved_bytes_disabled(&[0x06, 0, 0, 0]));
        assert!(!approved_bytes_disabled(&[]));
    }

    /// Scratch registry subtree removed on drop, so tests never touch the real Run key.
    struct Scratch {
        root: String,
        run: String,
        approved: String,
    }
    impl Scratch {
        fn new(tag: &str) -> Self {
            let root = format!(r"Software\OpenTalkTest\{}-{tag}", std::process::id());
            Scratch { run: format!(r"{root}\Run"), approved: format!(r"{root}\Approved"), root }
        }
        fn keys(&self) -> Keys<'_> {
            Keys { run: &self.run, approved: &self.approved }
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            // SAFETY: plain registry calls with live strings.
            unsafe {
                let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(self.root.as_str()));
                // Remove the (now empty) parent if nothing else lives there.
                let _ = RegDeleteKeyW(HKEY_CURRENT_USER, &HSTRING::from(r"Software\OpenTalkTest"));
            }
        }
    }

    #[test]
    #[ignore = "desktop: writes HKCU scratch keys"]
    fn desktop_scratch_enable_disable_cycle() {
        let s = Scratch::new("cycle");
        let k = s.keys();
        let exe = r"C:\Temp\open talk\opentalk.exe";
        assert!(!is_enabled_in(&k));

        set_enabled_in(&k, true, exe).unwrap();
        assert!(is_enabled_in(&k));
        let (ty, data) = read_value(k.run, VALUE_NAME).unwrap();
        assert_eq!(ty, REG_SZ);
        let wide: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let text = String::from_utf16_lossy(&wide);
        assert_eq!(text.trim_end_matches('\0'), command_line(exe));

        // User disables it in Task Manager.
        write_value(k.approved, VALUE_NAME, REG_BINARY, &[0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        assert!(!is_enabled_in(&k), "StartupApproved 0x03 disables");
        // Re-enabling from our UI clears the flag.
        set_enabled_in(&k, true, exe).unwrap();
        assert!(is_enabled_in(&k));
        assert!(read_value(k.approved, VALUE_NAME).is_none());

        // An explicit "enabled" flag from Task Manager stays untouched.
        write_value(k.approved, VALUE_NAME, REG_BINARY, &[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        assert!(is_enabled_in(&k));
        set_enabled_in(&k, true, exe).unwrap();
        assert!(read_value(k.approved, VALUE_NAME).is_some());

        set_enabled_in(&k, false, exe).unwrap();
        assert!(!is_enabled_in(&k));
        // Disabling twice is fine.
        set_enabled_in(&k, false, exe).unwrap();
    }

    /// Exercises the real key (what `set_enabled` uses) and restores the prior state.
    #[test]
    #[ignore = "desktop: writes the real HKCU Run key (restored afterwards)"]
    fn desktop_real_key_round_trip_restores_state() {
        let prior_run = read_value(RUN_KEY, VALUE_NAME);
        let prior_approved = read_value(APPROVED_KEY, VALUE_NAME);

        let result = std::panic::catch_unwind(|| {
            set_enabled(true).unwrap();
            assert!(is_enabled());
            let exe = std::env::current_exe().unwrap();
            let (_, data) = read_value(RUN_KEY, VALUE_NAME).unwrap();
            let wide: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let text = String::from_utf16_lossy(&wide);
            assert_eq!(text.trim_end_matches('\0'), command_line(&exe.to_string_lossy()));
            set_enabled(false).unwrap();
            assert!(!is_enabled());
        });

        match prior_run {
            Some((ty, data)) => write_value(RUN_KEY, VALUE_NAME, ty, &data).unwrap(),
            None => delete_value(RUN_KEY, VALUE_NAME).unwrap(),
        }
        match prior_approved {
            Some((ty, data)) => write_value(APPROVED_KEY, VALUE_NAME, ty, &data).unwrap(),
            None => delete_value(APPROVED_KEY, VALUE_NAME).unwrap(),
        }
        if let Err(p) = result {
            std::panic::resume_unwind(p);
        }
    }
}
