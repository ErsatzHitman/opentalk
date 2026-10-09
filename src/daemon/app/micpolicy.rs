//! When the daemon may keep a microphone stream prepared between dictations.
//!
//! The audio engine keeps an initialised (not started) capture stream parked so a dictation only
//! pays for `IAudioClient::Start`. On wired and USB microphones that is invisible: Windows only
//! lights the mic indicator for a started stream. Initialising a capture stream on a Bluetooth
//! headset, however, can switch it to the hands-free profile (HFP) and degrade music or call audio
//! for as long as the stream exists. So Bluetooth endpoints are only kept prepared when the user
//! asked for a warm microphone (`mic_warm_hold_secs > 0`); otherwise the stream is released after
//! every recording and never prepared at start-up (PLAN §9.2: keep-warm is opt-in).
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_EnumeratorName;
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eConsole};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize, STGM_READ};
use windows::core::HSTRING;

use crate::daemon::audio;
use crate::log_info;

/// True when the PnP enumerator name belongs to a Bluetooth device (`BTHENUM`, `BTHHFENUM`,
/// `BTHLEDevice`, ...).
pub fn is_bluetooth_enumerator(name: &str) -> bool {
    name.trim().to_ascii_uppercase().starts_with("BTH")
}

/// PnP enumerator name of the capture endpoint `device_id` (None = default console capture
/// device). Blocks for a few ms (device enumeration); call it off the UI thread.
pub fn enumerator_name(device_id: Option<&str>) -> Option<String> {
    // SAFETY: COM is initialised for this call and balanced when it succeeded; interface calls
    // are on valid objects created here, and the PROPVARIANT is converted before it is dropped.
    unsafe {
        let init = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let name = (|| {
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let dev = match device_id {
                Some(id) => en.GetDevice(&HSTRING::from(id)).or_else(|_| en.GetDefaultAudioEndpoint(eCapture, eConsole)).ok()?,
                None => en.GetDefaultAudioEndpoint(eCapture, eConsole).ok()?,
            };
            let store = dev.OpenPropertyStore(STGM_READ).ok()?;
            let value = store.GetValue(&PKEY_Device_EnumeratorName).ok()?;
            Some(value.to_string())
        })();
        if init {
            CoUninitialize();
        }
        name.filter(|n| !n.is_empty())
    }
}

/// Whether a stream for `device_id` may stay prepared between dictations.
pub fn may_keep_prepared(device_id: Option<&str>, warm_hold_secs: u32) -> bool {
    if warm_hold_secs > 0 || audio::inject_path().is_some() {
        return true;
    }
    !enumerator_name(device_id).is_some_and(|n| is_bluetooth_enumerator(&n))
}

/// Start-up / microphone change: prepare the stream in the background unless the endpoint is a
/// Bluetooth headset (see the module docs). Runs on its own short-lived thread.
pub fn prepare_in_background(device_id: Option<String>, warm_hold_secs: u32) {
    let spawned = std::thread::Builder::new().name("mic-prepare".into()).stack_size(256 * 1024).spawn(move || {
        if may_keep_prepared(device_id.as_deref(), warm_hold_secs) {
            match device_id.as_deref() {
                Some(id) => audio::Recorder::prepare(Some(id)),
                None => audio::warmup(),
            }
        } else {
            log_info!("audio: Bluetooth microphone; not keeping a stream prepared (keep-warm is off)");
            audio::Recorder::release_prepared();
        }
    });
    if spawned.is_err() {
        // Without the check thread, fall back to the plain warm-up (never fatal).
        audio::warmup();
    }
}

/// After a recording ended (worker thread): drop the parked stream again on Bluetooth endpoints.
pub fn after_recording(device_id: Option<&str>, warm_hold_secs: u32) {
    if !may_keep_prepared(device_id, warm_hold_secs) {
        audio::Recorder::release_prepared();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bluetooth_enumerators() {
        for bt in ["BTHENUM", "BTHHFENUM", "BTHLEDevice", "bthenum ", "BTHLE"] {
            assert!(is_bluetooth_enumerator(bt), "{bt}");
        }
        for wired in ["USB", "HDAUDIO", "INTELAUDIO", "SWD", "MMDEVAPI", ""] {
            assert!(!is_bluetooth_enumerator(wired), "{wired}");
        }
    }

    #[test]
    fn keep_warm_always_allows_preparing() {
        assert!(may_keep_prepared(Some("{not-a-real-endpoint}"), 30));
    }

    #[test]
    fn default_capture_endpoint_has_an_enumerator() {
        // Machines without a microphone (CI) have no default endpoint: nothing to check then.
        if let Some(name) = enumerator_name(None) {
            assert!(!name.trim().is_empty());
        }
    }
}
