//! Pure dictation state machine (no Win32): turns inputs (hotkey edges, timers, clicks, IPC) into
//! one action for the host to perform. Unit-tested without a desktop.
//!
//! ```text
//! Idle --down(tap)--> Recording --down/click/toggle--> Thinking --job done--> Idle
//! Idle --down(hold)--> Arming --150 ms--> Recording(hold) --up--> Thinking
//!                        '--up (early)--> Idle (cancelled)
//! Recording/Thinking --Esc/cancel--> Idle
//! ```
use crate::core::config::ActivationMode;

/// Hold mode only starts recording after the key has been down this long.
pub const ARM_MS: u32 = 150;
/// Safety cap on one recording.
pub const MAX_RECORDING_MS: u32 = 5 * 60 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    /// Hold mode, key is down, waiting for [`ARM_MS`].
    Arming,
    /// `hold`: started by a held hotkey, so the key release stops it.
    Recording { hold: bool },
    /// Transcribing / cleaning / pasting.
    Thinking,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    HotkeyDown,
    HotkeyUp,
    /// The arming timer fired.
    ArmTimer,
    /// The recording safety cap fired.
    CapTimer,
    /// Esc, swallowed by the hook while recording or thinking.
    Escape,
    PillClick,
    CancelClick,
    /// Tray / IPC / menu "toggle listening".
    Toggle,
    /// IPC cancel.
    Cancel,
    /// The worker finished (or the stop found nothing to transcribe).
    JobDone,
    /// Starting the recorder failed.
    StartFailed,
    /// "Retry last" on kept audio: go straight to transcribing.
    RetryStart,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    /// Arm the 150 ms timer.
    BeginArming,
    /// Hold released early: stop the arming timer, nothing was shown.
    CancelArming,
    StartRecording { hold: bool },
    StopRecording,
    CancelRecording,
    /// Abandon the in-flight transcription (its result is dropped).
    CancelJob,
}

#[derive(Clone, Copy, Debug)]
pub struct Machine {
    pub phase: Phase,
    pub mode: ActivationMode,
}

impl Machine {
    pub fn new(mode: ActivationMode) -> Machine {
        Machine { phase: Phase::Idle, mode }
    }

    pub fn is_recording(&self) -> bool {
        matches!(self.phase, Phase::Recording { .. })
    }

    /// Recording or transcribing (the Esc key is ours in these phases).
    pub fn is_busy(&self) -> bool {
        matches!(self.phase, Phase::Recording { .. } | Phase::Thinking)
    }

    pub fn step(&mut self, input: Input) -> Action {
        use Action as A;
        use Input as I;
        use Phase as P;
        match (self.phase, input) {
            (P::Idle, I::HotkeyDown) => match self.mode {
                ActivationMode::Tap => {
                    self.phase = P::Recording { hold: false };
                    A::StartRecording { hold: false }
                }
                ActivationMode::Hold => {
                    self.phase = P::Arming;
                    A::BeginArming
                }
            },
            (P::Idle, I::PillClick | I::Toggle) => {
                self.phase = P::Recording { hold: false };
                A::StartRecording { hold: false }
            }
            (P::Idle, I::RetryStart) => {
                self.phase = P::Thinking;
                A::None
            }

            (P::Arming, I::HotkeyUp | I::Cancel | I::Escape) => {
                self.phase = P::Idle;
                A::CancelArming
            }
            (P::Arming, I::ArmTimer) => {
                self.phase = P::Recording { hold: true };
                A::StartRecording { hold: true }
            }

            (P::Recording { hold }, I::HotkeyDown) if !hold => self.stop(),
            (P::Recording { hold }, I::HotkeyUp) if hold => self.stop(),
            (P::Recording { .. }, I::PillClick | I::Toggle | I::CapTimer) => self.stop(),
            (P::Recording { .. }, I::Escape | I::CancelClick | I::Cancel) => {
                self.phase = P::Idle;
                A::CancelRecording
            }

            (P::Thinking, I::Escape | I::CancelClick | I::Cancel) => {
                self.phase = P::Idle;
                A::CancelJob
            }
            (P::Thinking, I::JobDone) => {
                self.phase = P::Idle;
                A::None
            }

            // The recorder never came up: back to idle from wherever we were heading.
            (P::Recording { .. } | P::Arming, I::StartFailed) => {
                self.phase = P::Idle;
                A::None
            }
            // Nothing to transcribe (no speech): the host reports it as a finished job.
            (P::Recording { .. }, I::JobDone) => {
                self.phase = P::Idle;
                A::None
            }
            _ => A::None,
        }
    }

    fn stop(&mut self) -> Action {
        self.phase = Phase::Thinking;
        Action::StopRecording
    }

    /// The configured mode changed (settings reload). Never interrupts a running dictation.
    pub fn set_mode(&mut self, mode: ActivationMode) {
        self.mode = mode;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action as A;
    use Input as I;

    fn tap() -> Machine {
        Machine::new(ActivationMode::Tap)
    }
    fn hold() -> Machine {
        Machine::new(ActivationMode::Hold)
    }

    #[test]
    fn tap_toggles_on_each_press() {
        let mut m = tap();
        assert_eq!(m.step(I::HotkeyDown), A::StartRecording { hold: false });
        assert_eq!(m.step(I::HotkeyUp), A::None, "release is ignored in tap mode");
        assert!(m.is_recording());
        assert_eq!(m.step(I::HotkeyDown), A::StopRecording);
        assert_eq!(m.phase, Phase::Thinking);
        assert_eq!(m.step(I::HotkeyDown), A::None, "presses while transcribing are ignored");
        assert_eq!(m.step(I::JobDone), A::None);
        assert_eq!(m.phase, Phase::Idle);
        assert_eq!(m.step(I::HotkeyDown), A::StartRecording { hold: false });
    }

    #[test]
    fn hold_arms_then_records_until_release() {
        let mut m = hold();
        assert_eq!(m.step(I::HotkeyDown), A::BeginArming);
        assert_eq!(m.step(I::HotkeyDown), A::None, "key repeat while arming");
        assert_eq!(m.step(I::ArmTimer), A::StartRecording { hold: true });
        assert_eq!(m.step(I::HotkeyDown), A::None, "key repeat while recording");
        assert!(m.is_recording());
        assert_eq!(m.step(I::HotkeyUp), A::StopRecording);
        assert_eq!(m.step(I::JobDone), A::None);
        assert_eq!(m.phase, Phase::Idle);
    }

    #[test]
    fn hold_released_early_cancels() {
        let mut m = hold();
        assert_eq!(m.step(I::HotkeyDown), A::BeginArming);
        assert_eq!(m.step(I::HotkeyUp), A::CancelArming);
        assert_eq!(m.phase, Phase::Idle);
        assert_eq!(m.step(I::ArmTimer), A::None, "a stale timer does nothing");
    }

    #[test]
    fn pill_click_in_hold_mode_records_like_tap() {
        let mut m = hold();
        assert_eq!(m.step(I::PillClick), A::StartRecording { hold: false });
        assert_eq!(m.step(I::HotkeyUp), A::None, "not started by the key: its release is ignored");
        assert_eq!(m.step(I::HotkeyDown), A::StopRecording, "a press stops it");
    }

    #[test]
    fn escape_and_cancel_abort_recording_and_thinking() {
        for first in [I::Escape, I::CancelClick, I::Cancel] {
            let mut m = tap();
            m.step(I::HotkeyDown);
            assert_eq!(m.step(first), A::CancelRecording, "{first:?}");
            assert_eq!(m.phase, Phase::Idle);
            m.step(I::HotkeyDown);
            m.step(I::HotkeyDown);
            assert_eq!(m.phase, Phase::Thinking);
            assert_eq!(m.step(first), A::CancelJob, "{first:?}");
            assert_eq!(m.phase, Phase::Idle);
        }
    }

    #[test]
    fn escape_when_idle_does_nothing() {
        let mut m = tap();
        assert_eq!(m.step(I::Escape), A::None);
        assert_eq!(m.step(I::CancelClick), A::None);
        assert_eq!(m.phase, Phase::Idle);
    }

    #[test]
    fn toggle_and_cap_stop_a_recording() {
        let mut m = tap();
        assert_eq!(m.step(I::Toggle), A::StartRecording { hold: false });
        assert_eq!(m.step(I::Toggle), A::StopRecording);
        let mut m = hold();
        m.step(I::HotkeyDown);
        m.step(I::ArmTimer);
        assert_eq!(m.step(I::CapTimer), A::StopRecording);
    }

    #[test]
    fn start_failure_and_empty_stop_return_to_idle() {
        let mut m = tap();
        m.step(I::HotkeyDown);
        assert_eq!(m.step(I::StartFailed), A::None);
        assert_eq!(m.phase, Phase::Idle);
        let mut m = tap();
        m.step(I::HotkeyDown);
        assert_eq!(m.step(I::JobDone), A::None);
        assert_eq!(m.phase, Phase::Idle);
    }

    #[test]
    fn retry_goes_straight_to_thinking() {
        let mut m = tap();
        assert_eq!(m.step(I::RetryStart), A::None);
        assert_eq!(m.phase, Phase::Thinking);
        assert_eq!(m.step(I::RetryStart), A::None, "only from idle");
        assert_eq!(m.step(I::JobDone), A::None);
        assert_eq!(m.phase, Phase::Idle);
    }

    #[test]
    fn stale_events_are_ignored_when_idle() {
        let mut m = tap();
        for i in [I::HotkeyUp, I::ArmTimer, I::CapTimer, I::JobDone, I::StartFailed, I::Cancel] {
            assert_eq!(m.step(i), A::None, "{i:?}");
            assert_eq!(m.phase, Phase::Idle);
        }
    }
}
