//! When the daemon may keep a microphone stream prepared between dictations.
//!
//! A prepared (initialised, not started) stream saves the driver's stream-initialisation cost on
//! the next `start`: 250-700 ms on some mic arrays (measured: ~500 ms on an Intel SST array),
//! against ~10 ms when the stream is already parked.
//!
//! A parked stream is not free, though: as long as it exists, the Windows audio engine
//! (audiodg.exe) keeps the endpoint's system effects (APOs) loaded, measured at ~230 MB on a
//! Realtek/Intel SST microphone. `mic_warm_hold_secs` therefore bounds how long the parked stream
//! lives after the last use: `0` releases it after every recording (lowest memory, slow first
//! start), `30`-`900` keep it warm that many seconds (fast back-to-back dictations, memory back
//! when the user pauses). Bluetooth headsets are no different: keeping their stream alive switches
//! them to the hands-free profile, and the same hold bounds that.
use crate::daemon::audio;

/// Whether a stream may stay prepared between dictations under this setting.
pub fn may_keep_prepared(warm_hold_secs: u32) -> bool {
    warm_hold_secs > 0 || audio::inject_path().is_some()
}

/// Prepare the stream for `device_id` (or release it) according to the keep-warm setting.
pub fn prepare_now(device_id: Option<&str>, warm_hold_secs: u32) {
    if may_keep_prepared(warm_hold_secs) {
        match device_id {
            Some(id) => audio::Recorder::prepare(Some(id), warm_hold_secs),
            None => audio::warmup(warm_hold_secs),
        }
    } else {
        audio::Recorder::release_prepared();
    }
}

/// [`prepare_now`] on a short-lived thread (microphone setting changed).
pub fn prepare_in_background(device_id: Option<String>, warm_hold_secs: u32) {
    let spawned = std::thread::Builder::new()
        .name("mic-prepare".into())
        .stack_size(256 * 1024)
        .spawn(move || prepare_now(device_id.as_deref(), warm_hold_secs));
    if spawned.is_err() {
        // Without the check thread, fall back to the plain warm-up (never fatal).
        if may_keep_prepared(warm_hold_secs) {
            audio::warmup(warm_hold_secs);
        }
    }
}

/// After a recording ended (worker thread): refresh the keep-warm window, or release the stream.
pub fn after_recording(warm_hold_secs: u32) {
    if may_keep_prepared(warm_hold_secs) {
        audio::Recorder::hold_prepared(warm_hold_secs);
    } else {
        audio::Recorder::release_prepared();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_keeps_the_stream_ready() {
        assert!(may_keep_prepared(1));
        assert!(may_keep_prepared(30));
        assert!(may_keep_prepared(900));
    }

    #[test]
    fn off_releases_the_stream() {
        // The injected-WAV test path keeps the recorder addresses stable; otherwise 0 means off.
        if audio::inject_path().is_none() {
            assert!(!may_keep_prepared(0));
        }
    }
}
