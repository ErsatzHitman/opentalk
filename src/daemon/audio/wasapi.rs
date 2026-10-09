//! Raw WASAPI shared-mode capture: cached device enumerator, event-driven capture thread,
//! OS-side conversion to 16 kHz mono i16 with an own downmix + resample fallback.
use super::resample::Resampler;
use super::{CaptureStats, Device, Output, SAMPLE_RATE, Stop};
use crate::{log_info, log_warn};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, PROPERTYKEY, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE, DEVICE_STATE_ACTIVE, EDataFlow, ERole, IAudioCaptureClient,
    IAudioClient, IMMDevice, IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eCapture,
    eConsole,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, STGM_READ,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, GetCurrentThread, GetThreadTimes, SetEvent,
    WaitForMultipleObjects, WaitForSingleObject,
};
use windows::Win32::System::WindowsProgramming::QueryThreadCycleTime;
use windows::core::{IUnknown, PCWSTR, PWSTR, implement, w};

/// Requested shared-mode buffer: 20 ms (100 ns units).
const BUFFER_100NS: i64 = 200_000;
/// Wake up this often even without packets, to notice a dead device.
const IDLE_WAKE_MS: u32 = 1000;

// ---------------------------------------------------------------------------------------------
// Win32 plumbing
// ---------------------------------------------------------------------------------------------

/// Owned event handle.
pub(crate) struct Ev(pub(crate) HANDLE);
// SAFETY: event handles are process-wide kernel objects usable from any thread.
unsafe impl Send for Ev {}
unsafe impl Sync for Ev {}

impl Ev {
    pub(crate) fn new(manual_reset: bool) -> Result<Ev, String> {
        // SAFETY: plain kernel call, no pointers other than null.
        unsafe { CreateEventW(None, manual_reset, false, PCWSTR::null()) }
            .map(Ev)
            .map_err(|e| format!("CreateEventW: {e}"))
    }
    pub(crate) fn set(&self) {
        // SAFETY: handle is valid for the lifetime of `self`.
        let _ = unsafe { SetEvent(self.0) };
    }
    /// Wait up to `ms`; true when signalled.
    pub(crate) fn wait(&self, ms: u32) -> bool {
        // SAFETY: handle is valid for the lifetime of `self`.
        unsafe { WaitForSingleObject(self.0, ms) == WAIT_OBJECT_0 }
    }
}

impl Drop for Ev {
    fn drop(&mut self) {
        // SAFETY: we own the handle and close it exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct ComGuard(bool);
impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

thread_local! {
    static COM: ComGuard = {
        // SAFETY: standard COM init; S_FALSE (already initialised) also needs the matching uninit.
        // RPC_E_CHANGED_MODE (thread is STA) is fine: COM is usable, we just must not uninit.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        ComGuard(hr.is_ok())
    };
}

/// Make sure COM is usable on the calling thread.
pub(crate) fn ensure_com() {
    COM.with(|_| {});
}

struct SendEnum(IMMDeviceEnumerator, #[allow(dead_code)] IMMNotificationClient);
// SAFETY: MMDeviceEnumerator is registered with ThreadingModel "Both"; the in-process object
// is callable from any apartment without marshalling. The notifier is a plain Rust object.
unsafe impl Send for SendEnum {}

/// Tells the engine when the capture device set or the default device changed, so the prepared
/// stream follows the user's microphone instead of making the next dictation slow.
#[implement(IMMNotificationClient)]
struct Notifier;

#[allow(non_snake_case)]
impl IMMNotificationClient_Impl for Notifier_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> windows::core::Result<()> {
        revalidate();
        Ok(())
    }
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        revalidate();
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _id: &PCWSTR) -> windows::core::Result<()> {
        if flow == eCapture && role == eConsole {
            revalidate();
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> windows::core::Result<()> {
        Ok(())
    }
}

static ENUMERATOR: Mutex<Option<SendEnum>> = Mutex::new(None);

fn enumerator() -> Result<IMMDeviceEnumerator, String> {
    ensure_com();
    let mut slot = ENUMERATOR.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(e) = slot.as_ref() {
        return Ok(e.0.clone());
    }
    // SAFETY: CLSID/IID come from the windows crate; no outer unknown.
    let e: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL) }.map_err(|e| format!("device enumerator: {e}"))?;
    let notifier: IMMNotificationClient = Notifier.into();
    // SAFETY: the notifier is kept alive next to the enumerator for the life of the process.
    if let Err(err) = unsafe { e.RegisterEndpointNotificationCallback(&notifier) } {
        log_warn!("audio: cannot watch for device changes: {err}");
    }
    *slot = Some(SendEnum(e.clone(), notifier));
    Ok(e)
}

fn take_pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: `p` is a NUL-terminated string allocated by COM; freed right after copying.
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn device_id(d: &IMMDevice) -> String {
    // SAFETY: GetId returns a CoTaskMem string we free in take_pwstr.
    unsafe { d.GetId() }.map(take_pwstr).unwrap_or_default()
}

pub(crate) fn list_devices() -> Vec<Device> {
    let Ok(en) = enumerator() else { return Vec::new() };
    // SAFETY: interface calls on valid objects; strings are freed after copying.
    unsafe {
        let default_id = en.GetDefaultAudioEndpoint(eCapture, eConsole).map(|d| device_id(&d)).unwrap_or_default();
        let Ok(coll) = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else { return Vec::new() };
        let n = coll.GetCount().unwrap_or(0);
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let Ok(dev) = coll.Item(i) else { continue };
            let id = device_id(&dev);
            if id.is_empty() {
                continue;
            }
            let name = dev
                .OpenPropertyStore(STGM_READ)
                .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
                .map(|pv| pv.to_string())
                .unwrap_or_default();
            let name = if name.is_empty() { id.clone() } else { name };
            out.push(Device { is_default: id == default_id, id, name });
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Sample conversion
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SampleFmt {
    F32,
    I16,
    I24,
    I32,
}

impl SampleFmt {
    fn bytes(self) -> usize {
        match self {
            SampleFmt::F32 | SampleFmt::I32 => 4,
            SampleFmt::I24 => 3,
            SampleFmt::I16 => 2,
        }
    }

    /// Decode sample `idx` of an interleaved little-endian buffer to [-1, 1].
    #[inline]
    fn decode(self, data: &[u8], idx: usize) -> f32 {
        let o = idx * self.bytes();
        match self {
            SampleFmt::F32 => f32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]),
            SampleFmt::I16 => i16::from_le_bytes([data[o], data[o + 1]]) as f32 / 32768.0,
            SampleFmt::I24 => (i32::from_le_bytes([0, data[o], data[o + 1], data[o + 2]]) >> 8) as f32 / 8_388_608.0,
            SampleFmt::I32 => i32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]) as f32 / 2_147_483_648.0,
        }
    }
}

#[inline]
pub(crate) fn f32_to_i16(x: f32) -> i16 {
    (x * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

/// Turns raw WASAPI packets into 16 kHz mono i16.
pub(crate) enum Converter {
    /// The OS already delivers 16 kHz mono i16.
    Native { scratch: Vec<i16> },
    /// Mix-format packets: downmix by averaging channels, then resample.
    Convert { fmt: SampleFmt, channels: usize, rate: u32, resampler: Option<Resampler>, mono: Vec<f32>, resampled: Vec<f32>, scratch: Vec<i16> },
}

impl Converter {
    pub(crate) fn convert(fmt: SampleFmt, channels: usize, rate: u32) -> Converter {
        Converter::Convert {
            fmt,
            channels: channels.max(1),
            rate,
            resampler: Resampler::new(rate, SAMPLE_RATE),
            mono: Vec::new(),
            resampled: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// Convert one packet (`data` holds `frames` interleaved frames, or is empty when `silent`)
    /// and append the result to `out`.
    pub(crate) fn push(&mut self, data: &[u8], frames: usize, silent: bool, out: &mut Output) {
        match self {
            Converter::Native { scratch } => {
                scratch.clear();
                if silent || data.len() < frames * 2 {
                    scratch.resize(frames, 0);
                } else {
                    scratch.extend(data[..frames * 2].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
                }
                out.push(scratch);
            }
            Converter::Convert { fmt, channels, resampler, mono, resampled, scratch, .. } => {
                mono.clear();
                if silent || data.len() < frames * *channels * fmt.bytes() {
                    mono.resize(frames, 0.0);
                } else {
                    let inv = 1.0 / *channels as f32;
                    for f in 0..frames {
                        let mut acc = 0.0f32;
                        for c in 0..*channels {
                            acc += fmt.decode(data, f * *channels + c);
                        }
                        mono.push(acc * inv);
                    }
                }
                scratch.clear();
                match resampler {
                    Some(r) => {
                        resampled.clear();
                        r.process(mono, resampled);
                        scratch.extend(resampled.iter().map(|&x| f32_to_i16(x)));
                    }
                    None => scratch.extend(mono.iter().map(|&x| f32_to_i16(x))),
                }
                out.push(scratch);
            }
        }
    }

    /// Forget resampler history (new recording on a reused stream).
    pub(crate) fn reset(&mut self) {
        if let Converter::Convert { resampler, rate, .. } = self {
            *resampler = Resampler::new(*rate, SAMPLE_RATE);
        }
    }

    /// Flush filter state at the end of the stream.
    fn finish(&mut self, out: &mut Output) {
        if let Converter::Convert { resampler: Some(r), resampled, scratch, .. } = self {
            resampled.clear();
            r.finish(resampled);
            scratch.clear();
            scratch.extend(resampled.iter().map(|&x| f32_to_i16(x)));
            out.push(scratch);
        }
    }
}

/// Decode a WAVEFORMATEX(/EXTENSIBLE) into (sample format, channels, rate).
///
/// # Safety
/// `p` must point to a valid WAVEFORMATEX, followed by the extension when `cbSize >= 22`.
unsafe fn parse_format(p: *const WAVEFORMATEX) -> Result<(SampleFmt, usize, u32), String> {
    // SAFETY: caller guarantees validity; the struct is packed so read unaligned.
    let f = unsafe { p.read_unaligned() };
    let (tag, bits, channels, rate) = (f.wFormatTag as u32, f.wBitsPerSample, f.nChannels as usize, f.nSamplesPerSec);
    let float = if tag == WAVE_FORMAT_EXTENSIBLE && f.cbSize >= 22 {
        // SAFETY: cbSize says the extension is present.
        let e = unsafe { (p as *const WAVEFORMATEXTENSIBLE).read_unaligned() };
        let sub = e.SubFormat;
        if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            true
        } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
            false
        } else {
            return Err(format!("unsupported sub-format {sub:?}"));
        }
    } else if tag == WAVE_FORMAT_IEEE_FLOAT {
        true
    } else if tag == WAVE_FORMAT_PCM {
        false
    } else {
        return Err(format!("unsupported format tag {tag}"));
    };
    let fmt = match (float, bits) {
        (true, 32) => SampleFmt::F32,
        (false, 16) => SampleFmt::I16,
        (false, 24) => SampleFmt::I24,
        (false, 32) => SampleFmt::I32,
        _ => return Err(format!("unsupported sample format float={float} bits={bits}")),
    };
    if channels == 0 || rate == 0 {
        return Err("mix format reports zero channels or rate".into());
    }
    Ok((fmt, channels, rate))
}

// ---------------------------------------------------------------------------------------------
// Stream setup
// ---------------------------------------------------------------------------------------------

/// An initialised (but possibly stopped) capture stream on one endpoint.
struct Stream {
    // Field order matters: interfaces drop before the event handle they signal.
    capture: IAudioCaptureClient,
    client: IAudioClient,
    event: Ev,
    conv: Converter,
    block_align: usize,
    desc: String,
    /// Endpoint id this stream captures from.
    endpoint_id: String,
    activate_ms: f64,
    init_ms: f64,
}

fn activate(device: &IMMDevice) -> Result<IAudioClient, String> {
    // SAFETY: activating the documented IAudioClient interface.
    unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }.map_err(|e| format!("Activate(IAudioClient): {e}"))
}

fn open_stream(device: &IMMDevice) -> Result<Stream, String> {
    const FLAGS: u32 = AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    let want = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: 1,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * 2,
        nBlockAlign: 2,
        wBitsPerSample: 16,
        cbSize: 0,
    };

    // Attempt 1: let the OS convert to exactly what we need.
    let t = Instant::now();
    let client = activate(device)?;
    let activate_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    // SAFETY: `want` outlives the call.
    let first = unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, FLAGS, BUFFER_100NS, 0, &want, None) };
    let (client, conv, block_align, desc) = match first {
        Ok(()) => (client, Converter::Native { scratch: Vec::new() }, 2usize, "16000 Hz mono s16 (OS-converted)".to_string()),
        Err(e) => {
            log_warn!("audio: AUTOCONVERTPCM 16k mono refused ({e}); falling back to mix format + own resampler");
            // A failed Initialize can leave the client unusable: start over with a fresh one.
            let client = activate(device)?;
            // SAFETY: GetMixFormat returns a CoTaskMem block we free below.
            let mix = unsafe { client.GetMixFormat() }.map_err(|e| format!("GetMixFormat: {e}"))?;
            // SAFETY: `mix` is a valid format block until freed.
            let parsed = unsafe { parse_format(mix) };
            let init = parsed.as_ref().ok().map(|_| {
                // SAFETY: `mix` valid; try the 20 ms buffer first, then the engine default.
                let r = unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, BUFFER_100NS, 0, mix, None) };
                if r.is_ok() {
                    return r;
                }
                unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 0, 0, mix, None) }
            });
            // SAFETY: allocated by GetMixFormat, freed exactly once.
            unsafe { CoTaskMemFree(Some(mix as *const _)) };
            let (fmt, channels, rate) = parsed?;
            init.ok_or("no mix format")?.map_err(|e| format!("Initialize(mix format): {e}"))?;
            let desc = format!("{rate} Hz {channels}ch {fmt:?} (own downmix/resample)");
            (client, Converter::convert(fmt, channels, rate), channels * fmt.bytes(), desc)
        }
    };
    let init_ms = t.elapsed().as_secs_f64() * 1000.0;

    let event = Ev::new(false)?;
    // SAFETY: event handle outlives the client (field order in `Stream`).
    unsafe { client.SetEventHandle(event.0) }.map_err(|e| format!("SetEventHandle: {e}"))?;
    // SAFETY: documented service interface of an initialised capture client.
    let capture: IAudioCaptureClient = unsafe { client.GetService() }.map_err(|e| format!("GetService(IAudioCaptureClient): {e}"))?;
    Ok(Stream { capture, client, event, conv, block_align, desc, endpoint_id: String::new(), activate_ms, init_ms })
}

/// The endpoint a request resolves to: the requested id when it is still an active device,
/// otherwise the default console capture device.
fn resolve_endpoint(en: &IMMDeviceEnumerator, requested: Option<&str>) -> Result<IMMDevice, String> {
    if let Some(id) = requested.filter(|s| !s.is_empty()) {
        let ids: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `ids` is NUL-terminated and outlives the call.
        match unsafe { en.GetDevice(PCWSTR(ids.as_ptr())) } {
            // SAFETY: plain state query.
            Ok(d) if unsafe { d.GetState() }.map(|s| s == DEVICE_STATE_ACTIVE).unwrap_or(false) => return Ok(d),
            Ok(_) => log_warn!("audio: selected microphone is not active; using the default one"),
            Err(e) => log_warn!("audio: selected microphone not found ({e}); using the default one"),
        }
    }
    // SAFETY: plain query.
    unsafe { en.GetDefaultAudioEndpoint(eCapture, eConsole) }.map_err(|e| format!("no default capture device: {e}"))
}

/// Open a stream for the request, falling back to the default device if the chosen one fails.
fn open_any(requested: Option<&str>) -> Result<Stream, String> {
    let en = enumerator()?;
    let dev = resolve_endpoint(&en, requested)?;
    let id = device_id(&dev);
    match open_stream(&dev) {
        Ok(mut s) => {
            s.endpoint_id = id;
            Ok(s)
        }
        Err(first) => {
            log_warn!("audio: device {id} failed ({first}); trying the default capture device");
            // SAFETY: plain query.
            let def = unsafe { en.GetDefaultAudioEndpoint(eCapture, eConsole) }.map_err(|e| format!("{first}; default device: {e}"))?;
            let def_id = device_id(&def);
            if def_id == id {
                return Err(first);
            }
            let mut s = open_stream(&def).map_err(|e| format!("{first}; default device: {e}"))?;
            s.endpoint_id = def_id;
            Ok(s)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Engine thread
//
// Initialising a shared-mode stream costs 250-700 ms on some drivers (measured on an Intel SST
// mic array), while Start/Stop cost ~10 ms / <1 ms. An initialised stream that is *not started*
// does not light the Windows microphone indicator (the ConsentStore LastUsedTimeStop stays set),
// so one thread keeps the stream initialised and idle between recordings; `start` then only
// pays for `IAudioClient::Start`.
// ---------------------------------------------------------------------------------------------

/// A recording request handed to the engine thread.
pub(crate) struct StartReq {
    pub device: Option<String>,
    pub stop: Arc<Stop>,
    pub out: Output,
    pub ready: Sender<Result<f64, String>>,
    pub done: Sender<CaptureStats>,
    pub t0: Instant,
}

enum Cmd {
    Prepare(Option<String>),
    /// Re-check the prepared stream against the current devices.
    Revalidate,
    Release,
    Start(Box<StartReq>),
}

static ENGINE: Mutex<Option<Sender<Cmd>>> = Mutex::new(None);

fn send(cmd: Cmd) -> Result<(), String> {
    let mut slot = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    let mut cmd = Some(cmd);
    for _ in 0..2 {
        if slot.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("opentalk-audio".into())
                .spawn(move || engine_main(rx))
                .map_err(|e| format!("cannot spawn audio thread: {e}"))?;
            *slot = Some(tx);
        }
        let Some(tx) = slot.as_ref() else { break };
        match tx.send(cmd.take().unwrap_or(Cmd::Release)) {
            Ok(()) => return Ok(()),
            // Engine thread died (should not happen): drop the dead channel and respawn once.
            Err(e) => {
                cmd = Some(e.0);
                *slot = None;
            }
        }
    }
    Err("audio thread unavailable".into())
}

/// Warm COM + the device enumerator and initialise the default microphone stream in the
/// background, so the first `start` only pays for `IAudioClient::Start`. Returns immediately and
/// never opens the microphone for capture (no mic indicator).
pub(crate) fn warmup() {
    let _ = send(Cmd::Prepare(None));
}

/// Device set / default changed: let a running engine re-check its prepared stream. Does not
/// spawn the engine (nothing to keep fresh if audio was never used).
fn revalidate() {
    let slot = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tx) = slot.as_ref() {
        let _ = tx.send(Cmd::Revalidate);
    }
}

/// Initialise `device` (None = default) in the background and keep it ready.
pub(crate) fn prepare(device: Option<&str>) {
    let _ = send(Cmd::Prepare(device.map(str::to_owned)));
}

/// Drop the idle prepared stream (the next `start` will be slow again).
pub(crate) fn release() {
    let _ = send(Cmd::Release);
}

/// Queue a recording on the engine thread.
pub(crate) fn start(req: StartReq) -> Result<(), String> {
    send(Cmd::Start(Box::new(req)))
}

fn engine_main(rx: std::sync::mpsc::Receiver<Cmd>) {
    ensure_com();
    let mut prepared: Option<Stream> = None;
    // What the user last asked to be kept ready (None = default device).
    let mut wanted: Option<String> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Prepare(dev) => {
                wanted = dev;
                prepare_now(&mut prepared, wanted.as_deref());
            }
            Cmd::Revalidate => {
                // Only follow device changes when something is being kept ready.
                if prepared.is_some() {
                    prepare_now(&mut prepared, wanted.as_deref());
                }
            }
            Cmd::Release => prepared = None,
            Cmd::Start(req) => {
                wanted = req.device.clone();
                run_session(&mut prepared, *req);
            }
        }
    }
}

fn prepare_now(prepared: &mut Option<Stream>, device: Option<&str>) {
    let t = Instant::now();
    match ensure_prepared(prepared, device) {
        Ok(true) => log_info!("audio: stream prepared in {:.1} ms", t.elapsed().as_secs_f64() * 1000.0),
        Ok(false) => {}
        Err(e) => log_warn!("audio: prepare failed: {e}"),
    }
}

/// Make sure `prepared` captures from the endpoint `requested` resolves to. True when it had to
/// (re)initialise.
fn ensure_prepared(prepared: &mut Option<Stream>, requested: Option<&str>) -> Result<bool, String> {
    let en = enumerator()?;
    let want = device_id(&resolve_endpoint(&en, requested)?);
    if prepared.as_ref().is_some_and(|s| s.endpoint_id == want) {
        return Ok(false);
    }
    // Old stream (wrong device) is dropped only after the new one is up; dropping is slow.
    let stale = prepared.take();
    let stream = open_any(requested)?;
    *prepared = Some(stream);
    drop(stale);
    Ok(true)
}

fn filetime_ms(ft: &FILETIME) -> f64 {
    ((ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64) as f64 / 10_000.0
}

/// Kernel + user time of the calling thread, ms. Tick-granular on Windows; see `thread_cycles`.
fn thread_cpu_ms() -> f64 {
    let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: out-pointers reference live locals; pseudo-handle for the current thread.
    if unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) }.is_ok() {
        filetime_ms(&k) + filetime_ms(&u)
    } else {
        0.0
    }
}

/// CPU cycles consumed by the calling thread (exact, unlike `GetThreadTimes`).
pub(crate) fn thread_cycles() -> u64 {
    let mut c = 0u64;
    // SAFETY: out-pointer references a live local; pseudo-handle for the current thread.
    if unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c) }.is_ok() { c } else { 0 }
}

/// Cycles per millisecond on this CPU right now, from a 3 ms busy loop on the calling thread.
pub(crate) fn cycles_per_ms() -> f64 {
    let (c0, t0) = (thread_cycles(), Instant::now());
    while t0.elapsed() < std::time::Duration::from_millis(3) {
        std::hint::spin_loop();
    }
    (thread_cycles() - c0) as f64 / (t0.elapsed().as_secs_f64() * 1000.0)
}

struct PacketClock {
    last: Option<Instant>,
    sum_ms: f64,
    max_ms: f64,
    n: u64,
}

impl PacketClock {
    fn tick(&mut self, stats: &mut CaptureStats) {
        let now = Instant::now();
        if let Some(prev) = self.last {
            let gap = now.duration_since(prev).as_secs_f64() * 1000.0;
            self.sum_ms += gap;
            self.max_ms = self.max_ms.max(gap);
            self.n += 1;
        }
        self.last = Some(now);
        stats.packets += 1;
    }
}

/// Drain every pending packet into `out`.
fn drain(stream: &mut Stream, out: &mut Output, clock: &mut PacketClock, stats: &mut CaptureStats) -> Result<(), windows::core::Error> {
    loop {
        // SAFETY: capture client is initialised and started.
        let pending = unsafe { stream.capture.GetNextPacketSize() }?;
        if pending == 0 {
            return Ok(());
        }
        let (mut data, mut frames, mut flags) = (std::ptr::null_mut::<u8>(), 0u32, 0u32);
        // SAFETY: out-pointers reference live locals.
        unsafe { stream.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }?;
        clock.tick(stats);
        let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
        let bytes = if silent {
            &[][..]
        } else {
            // SAFETY: WASAPI guarantees `frames * nBlockAlign` readable bytes until ReleaseBuffer.
            unsafe { std::slice::from_raw_parts(data, frames as usize * stream.block_align) }
        };
        stream.conv.push(bytes, frames as usize, silent, out);
        // SAFETY: releases exactly what GetBuffer returned.
        unsafe { stream.capture.ReleaseBuffer(frames) }?;
        if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
            stats.discontinuities += 1;
        }
    }
}

/// Start the prepared stream, (re)initialising it first when missing or stale.
fn start_stream(prepared: &mut Option<Stream>, device: Option<&str>) -> Result<(), String> {
    for attempt in 0..2 {
        ensure_prepared(prepared, device)?;
        let Some(stream) = prepared.as_mut() else { return Err("no stream".into()) };
        stream.conv.reset();
        // SAFETY: client initialised.
        match unsafe { stream.client.Start() } {
            Ok(()) => return Ok(()),
            Err(e) => {
                log_warn!("audio: IAudioClient::Start failed ({e}); re-initialising");
                *prepared = None;
                if attempt == 1 {
                    return Err(format!("IAudioClient::Start: {e}"));
                }
            }
        }
    }
    Err("IAudioClient::Start failed".into())
}

/// One recording: start, capture until `req.stop`, report, then park the stream stopped.
fn run_session(prepared: &mut Option<Stream>, req: StartReq) {
    let StartReq { device, stop, mut out, ready, done, t0 } = req;
    let mut stats = CaptureStats { backend: "wasapi", ..CaptureStats::default() };
    if stop.wait(0) {
        // The caller gave up (timeout / cancel) before we got to it.
        let _ = done.send(stats);
        return;
    }
    let was_prepared = prepared.is_some();
    let t_start = Instant::now();
    if let Err(e) = start_stream(prepared, device.as_deref()) {
        let _ = ready.send(Err(e));
        let _ = done.send(stats);
        return;
    }
    let start_ms = t_start.elapsed().as_secs_f64() * 1000.0;
    let Some(mut stream) = prepared.take() else { return };
    stats.activation_ms = t0.elapsed().as_secs_f64() * 1000.0;
    stats.format = stream.desc.clone();
    log_info!(
        "audio: capture started in {:.1} ms ({} stream: start {:.1} ms, Activate {:.1} + Initialize {:.1} when opened) {}",
        stats.activation_ms,
        if was_prepared { "prepared" } else { "cold" },
        start_ms,
        stream.activate_ms,
        stream.init_ms,
        stream.desc
    );
    let _ = ready.send(Ok(stats.activation_ms));

    // Optional: ask MMCSS for audio scheduling so a busy system cannot starve the capture loop.
    let mut task_index = 0u32;
    // SAFETY: valid task name and out-pointer.
    let mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) }.ok();

    let (cpu0, cyc0) = (thread_cpu_ms(), thread_cycles());
    let mut clock = PacketClock { last: None, sum_ms: 0.0, max_ms: 0.0, n: 0 };
    let mut lost = false;
    let mut retried = false;
    loop {
        let handles = [stop.event_handle(), stream.event.0];
        // Stop is listed first so it wins when both are signalled.
        // SAFETY: both handles are valid for the loop's lifetime.
        let r = unsafe { WaitForMultipleObjects(&handles, false, IDLE_WAKE_MS) };
        let stopping = r == WAIT_OBJECT_0;
        if !stopping && r.0 != WAIT_OBJECT_0.0 + 1 && r != WAIT_TIMEOUT {
            log_warn!("audio: wait failed ({r:?}); ending capture");
            lost = true;
            break;
        }
        // Always drain: after stop it picks up the last packets; on timeout it detects a dead device.
        if let Err(e) = drain(&mut stream, &mut out, &mut clock, &mut stats) {
            // A parked stream can go stale (device format changed, driver reset) while `Start`
            // still succeeds. If nothing was captured yet, open a fresh stream once and carry on.
            if out.len() == 0 && was_prepared && !retried && !stopping {
                retried = true;
                log_warn!("audio: parked stream failed on first read ({e}); re-opening the device");
                if let Ok(fresh) = start_stream(prepared, device.as_deref()).and_then(|()| prepared.take().ok_or_else(|| "no stream".to_string())) {
                    stream = fresh;
                    continue;
                }
            }
            if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
                log_warn!("audio: capture device invalidated (unplugged/disabled); keeping {} ms captured", out.len() / 16);
            } else {
                log_warn!("audio: capture error: {e}");
            }
            lost = true;
            break;
        }
        if stopping {
            break;
        }
    }
    stream.conv.finish(&mut out);
    stats.thread_cpu_ms = thread_cpu_ms() - cpu0;
    stats.thread_cycles = thread_cycles().saturating_sub(cyc0);
    if clock.n > 0 {
        stats.mean_gap_ms = clock.sum_ms / clock.n as f64;
        stats.max_gap_ms = clock.max_ms;
    }
    if lost {
        out.mark_lost();
        stats.device_lost = true;
    }
    // The caller is waiting for this: report before the (cheap, but not free) teardown.
    let _ = done.send(stats);
    drop(out);

    // SAFETY: stopping/resetting an initialised client; errors only mean the stream is stale.
    let parked = unsafe { stream.client.Stop().and_then(|()| stream.client.Reset()) };
    if let Some(h) = mmcss {
        // SAFETY: handle returned by AvSetMmThreadCharacteristicsW on this thread.
        let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
    }
    if lost || parked.is_err() {
        // Slow to drop (~100 ms), but the caller already has its audio.
        drop(stream);
    } else {
        *prepared = Some(stream);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    fn out() -> Output {
        Output::new(Arc::new(AtomicU32::new(0)), None, Arc::new(super::super::Shared::default()))
    }

    fn f32_bytes(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    /// The GUI thread is an STA; device listing must work there and agree with an MTA caller.
    #[test]
    #[ignore]
    fn desktop_list_devices_from_sta_and_mta_threads() {
        use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
        let sta = std::thread::spawn(|| {
            // SAFETY: standard COM init on a fresh thread; the thread ends right after.
            let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            list_devices()
        })
        .join()
        .expect("STA thread panicked");
        let mta = std::thread::spawn(list_devices).join().expect("MTA thread panicked");
        assert_eq!(sta, mta);
        assert!(sta.iter().filter(|d| d.is_default).count() <= 1);
        assert!(sta.iter().all(|d| !d.id.is_empty() && !d.name.is_empty()));
    }

    #[test]
    fn stereo_float_downmix_averages_channels() {
        let mut c = Converter::convert(SampleFmt::F32, 2, 16_000);
        let mut o = out();
        // L=0.5, R=-0.1 -> 0.2
        let data = f32_bytes(&[0.5, -0.1, 0.5, -0.1]);
        c.push(&data, 2, false, &mut o);
        let s = o.snapshot();
        assert_eq!(s, vec![f32_to_i16(0.2), f32_to_i16(0.2)]);
    }

    #[test]
    fn silent_flag_yields_zeros() {
        let mut c = Converter::convert(SampleFmt::F32, 2, 16_000);
        let mut o = out();
        c.push(&[], 160, true, &mut o);
        let mut n = Converter::Native { scratch: Vec::new() };
        n.push(&[], 160, true, &mut o);
        let s = o.snapshot();
        assert_eq!(s.len(), 320);
        assert!(s.iter().all(|&x| x == 0));
    }

    #[test]
    fn int_formats_decode_to_full_scale() {
        let mut o = out();
        let mut c = Converter::convert(SampleFmt::I16, 1, 16_000);
        c.push(&i16::MAX.to_le_bytes(), 1, false, &mut o);
        let mut c = Converter::convert(SampleFmt::I24, 1, 16_000);
        c.push(&[0xFF, 0xFF, 0x7F], 1, false, &mut o); // 0x7FFFFF
        let mut c = Converter::convert(SampleFmt::I32, 1, 16_000);
        c.push(&i32::MIN.to_le_bytes(), 1, false, &mut o);
        assert_eq!(o.snapshot(), vec![32767, 32767, -32768]);
    }

    #[test]
    fn short_buffer_is_treated_as_silence_not_panic() {
        let mut c = Converter::convert(SampleFmt::F32, 2, 16_000);
        let mut o = out();
        c.push(&[1, 2, 3], 10, false, &mut o);
        assert_eq!(o.snapshot().len(), 10);
    }

    #[test]
    fn mix_format_48k_resamples_to_16k() {
        let mut c = Converter::convert(SampleFmt::F32, 2, 48_000);
        let mut o = out();
        let tone: Vec<f32> = (0..48_000).flat_map(|n| {
            let v = 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 48_000.0).sin();
            [v, v]
        }).collect();
        // Feed in 10 ms packets like WASAPI does.
        for chunk in tone.chunks(480 * 2) {
            c.push(&f32_bytes(chunk), chunk.len() / 2, false, &mut o);
        }
        c.finish(&mut o);
        let s = o.snapshot();
        assert!((s.len() as i64 - 16_000).abs() < 100, "{}", s.len());
        let mid = &s[2_000..s.len() - 2_000];
        let rms = (mid.iter().map(|&x| (x as f64 / 32768.0).powi(2)).sum::<f64>() / mid.len() as f64).sqrt();
        assert!((rms - 0.5 / 2f64.sqrt()).abs() < 0.01, "{rms}");
    }
}
