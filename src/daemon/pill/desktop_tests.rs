//! Desktop-interacting tests (real window, real timer thread). Run with
//! `cargo test desktop_ -- --ignored --test-threads=1`.

use super::*;
use windows::Win32::Foundation::RECT;
use std::time::{Duration, Instant};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetWindowRect, IsWindowVisible, MSG, PM_REMOVE, PeekMessageW, SendMessageW, TranslateMessage,
    WM_MOUSEMOVE as MOVE,
};

fn pump(ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        // SAFETY: standard message pump on the creating thread.
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn opts() -> PillOptions {
    PillOptions { position: PanelPosition::BottomRight, auto_hide: false, show_app_bubble: true, theme: Theme::Dark, hotkey_label: "Ctrl + Win".into() }
}

fn lparam(x: i32, y: i32) -> LPARAM {
    LPARAM((((y as isize) & 0xffff) << 16) | ((x as isize) & 0xffff))
}

fn rect(hwnd: HWND) -> RECT {
    let mut r = RECT::default();
    // SAFETY: valid window handle and out-pointer.
    unsafe {
        let _ = GetWindowRect(hwnd, &mut r);
    }
    r
}

type Events = Rc<RefCell<Vec<PillEvent>>>;

fn make() -> (Window, Events) {
    let events: Events = Rc::new(RefCell::new(Vec::new()));
    let e2 = events.clone();
    let w = Window::create(Arc::new(AtomicU32::new(0)), opts(), Box::new(move |e| e2.borrow_mut().push(e))).expect("create pill window");
    (w, events)
}

fn send(hwnd: HWND, msg: u32, wp: usize, lp: LPARAM) -> isize {
    // SAFETY: synthetic message to our own window on its own thread.
    unsafe { SendMessageW(hwnd, msg, Some(WPARAM(wp)), Some(lp)).0 }
}

#[test]
#[ignore]
fn desktop_pill_lifecycle_and_idle_is_silent() {
    let (w, _events) = make();
    let hwnd = HWND(w.hwnd() as *mut core::ffi::c_void);
    // SAFETY: valid handle.
    assert!(unsafe { IsWindowVisible(hwnd) }.as_bool(), "idle pill is visible");
    let (scale, work, size) = w.with(|i| (i.st.scale, i.work, i.client_size()));
    let r = rect(hwnd);
    assert_eq!((r.right - r.left, r.bottom - r.top), size);
    assert_eq!((r.left, r.top), place_window(work, size, PanelPosition::BottomRight, scale));

    w.with(|i| i.set_mode(PillMode::Recording));
    pump(300);
    assert!(w.with(|i| i.timer.as_ref().map(|t| t.is_active()).unwrap_or(false)), "timer runs while recording");
    let f0 = w.with(|i| i.frames);
    pump(500);
    let f1 = w.with(|i| i.frames);
    let fps = (f1 - f0) as f64 / 0.5;
    println!("recording frame rate: {fps:.1} fps");
    assert!(fps > 40.0 && fps < 80.0, "recording runs at ~60 fps, got {fps}");

    w.with(|i| i.set_mode(PillMode::Thinking));
    pump(600);
    w.with(|i| i.set_mode(PillMode::Idle));
    pump(800);
    assert!(!w.with(|i| i.timer.as_ref().map(|t| t.is_active()).unwrap_or(true)), "timer stopped at idle");
    let f2 = w.with(|i| i.frames);
    pump(600);
    assert_eq!(w.with(|i| i.frames), f2, "no frames while idle");
}

#[test]
#[ignore]
fn desktop_pill_click_drag_hover_hittest() {
    let (w, events) = make();
    let hwnd = HWND(w.hwnd() as *mut core::ffi::c_void);
    let (scale, g) = w.with(|i| (i.st.scale, i.st.geometry(i.now())));
    let (px, py) = ((g.pill.cx() * scale) as i32, (g.pill.cy() * scale) as i32);
    // click
    send(hwnd, WM_LBUTTONDOWN, 1, lparam(px, py));
    send(hwnd, WM_LBUTTONUP, 0, lparam(px, py));
    assert_eq!(events.borrow().as_slice(), &[PillEvent::Clicked]);
    events.borrow_mut().clear();

    // transparent corner passes clicks through, the pill does not
    let r = rect(hwnd);
    assert_eq!(send(hwnd, WM_NCHITTEST, 0, lparam(r.left + 2, r.top + 2)), HTTRANSPARENT as isize);
    assert_eq!(send(hwnd, WM_NCHITTEST, 0, lparam(r.left + px, r.top + py)), HTCLIENT as isize);

    // hover: look + tooltip state follow the pointer
    send(hwnd, MOVE, 0, lparam(px, py));
    assert!(w.with(|i| i.st.pointer().group && i.st.pointer().pill));
    send(hwnd, WM_MOUSELEAVE, 0, LPARAM(0));
    assert!(!w.with(|i| i.st.pointer().group));
    pump(300);

    // drag: > 5 px moves the window and does not click
    let start = rect(hwnd);
    send(hwnd, WM_LBUTTONDOWN, 1, lparam(px, py));
    // the cursor moves by (-60, -30) in screen space; client coords are relative to the moving window
    let target = (start.left + px - 60, start.top + py - 30);
    for _ in 0..2 {
        let now = rect(hwnd);
        send(hwnd, MOVE, 1, lparam(target.0 - now.left, target.1 - now.top));
    }
    let now = rect(hwnd);
    send(hwnd, WM_LBUTTONUP, 0, lparam(target.0 - now.left, target.1 - now.top));
    let end = rect(hwnd);
    assert_eq!((end.left - start.left, end.top - start.top), (-60, -30), "window followed the cursor");
    assert!(events.borrow().is_empty(), "a drag is not a click: {:?}", events.borrow());

    // a 3 px wobble is still a click
    let g = w.with(|i| i.st.geometry(i.now()));
    let (px, py) = ((g.pill.cx() * scale) as i32, (g.pill.cy() * scale) as i32);
    send(hwnd, WM_LBUTTONDOWN, 1, lparam(px, py));
    send(hwnd, WM_LBUTTONUP, 0, lparam(px + 3, py));
    assert_eq!(events.borrow().as_slice(), &[PillEvent::Clicked]);

    // re-applying the same options keeps the dragged spot; a new panel position re-docks the pill
    // (the dragged position is not persisted)
    w.with(|i| i.set_options(opts()));
    let kept = rect(hwnd);
    assert_eq!((kept.left - start.left, kept.top - start.top), (-60, -30), "unrelated option change keeps the drag");
    w.with(|i| i.set_options(PillOptions { position: PanelPosition::BottomLeft, ..opts() }));
    w.with(|i| i.set_options(opts()));
    let back = rect(hwnd);
    assert_eq!((back.left, back.top), (start.left, start.top));

    // capture lost mid-press (Alt+Tab): later moves are hovers, not a drag
    let g = w.with(|i| i.st.geometry(i.now()));
    let (px, py) = ((g.pill.cx() * scale) as i32, (g.pill.cy() * scale) as i32);
    send(hwnd, WM_LBUTTONDOWN, 1, lparam(px, py));
    send(hwnd, WM_CAPTURECHANGED, 0, LPARAM(0));
    send(hwnd, MOVE, 0, lparam(px - 40, py - 20));
    let after = rect(hwnd);
    assert_eq!((after.left, after.top), (start.left, start.top), "no drag after capture loss");
    send(hwnd, WM_LBUTTONUP, 0, lparam(px, py));
    assert!(events.borrow().len() == 1, "no click after capture loss: {:?}", events.borrow());
}

#[test]
#[ignore]
fn desktop_pill_cancel_button_and_error_card() {
    let (w, events) = make();
    let hwnd = HWND(w.hwnd() as *mut core::ffi::c_void);
    w.with(|i| i.set_mode(PillMode::Recording));
    pump(900);
    let scale = w.with(|i| i.st.scale);
    let g = w.with(|i| i.st.geometry(i.now()));
    let (px, py) = ((g.pill.cx() * scale) as i32, (g.pill.cy() * scale) as i32);
    // hover without touching the real cursor: mark leave-tracking as armed so the synthetic move is
    // not cancelled by an immediate WM_MOUSELEAVE (the cursor is elsewhere)
    w.with(|i| i.tracking = true);
    send(hwnd, MOVE, 0, lparam(px, py));
    pump(600);
    let g = w.with(|i| i.st.geometry(i.now()));
    assert!(g.cancel_t > 0.99, "cancel bud emerged (pointer {:?})", w.with(|i| i.st.pointer()));
    let (cx, cy) = ((g.cancel.0 * scale) as i32, (g.cancel.1 * scale) as i32);
    send(hwnd, WM_LBUTTONDOWN, 1, lparam(cx, cy));
    send(hwnd, WM_LBUTTONUP, 0, lparam(cx, cy));
    assert_eq!(events.borrow().as_slice(), &[PillEvent::CancelClicked]);
    send(hwnd, WM_MOUSELEAVE, 0, LPARAM(0));
    w.with(|i| i.set_mode(PillMode::Idle));
    pump(100);

    // error card: bigger window, auto-dismiss after 3 s, then back to the pill
    w.with(|i| i.show_error("Transcription failed", "The speech provider rejected your API key.", true));
    let r = rect(hwnd);
    assert_eq!(r.right - r.left, (466.0 * scale).ceil() as i32);
    pump(3600);
    assert!(events.borrow().contains(&PillEvent::ErrorDismissed), "auto-dismissed: {:?}", events.borrow());
    let r = rect(hwnd);
    assert_eq!(r.right - r.left, w.with(|i| i.client_size().0));
    assert_eq!(r.right - r.left, (208.0 * scale).ceil() as i32);
}

#[test]
#[ignore]
fn desktop_pill_frame_cost() {
    let (w, _events) = make();
    w.with(|i| i.set_mode(PillMode::Recording));
    pump(800);
    let n = 300;
    let t0 = Instant::now();
    w.with(|i| {
        for _ in 0..n {
            // drop the frame cache so every present is a full repaint (silent input would be skipped)
            i.cache.reset();
            i.present();
        }
    });
    let per = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
    println!("full frame (render + upload + UpdateLayeredWindow): {per:.3} ms");
    assert!(per < if cfg!(debug_assertions) { 15.0 } else { 1.0 }, "frame cost {per:.3} ms");
}

#[test]
#[ignore]
fn desktop_pill_real_app_icon_bubble() {
    let (w, _events) = make();
    let hwnd = HWND(w.hwnd() as *mut core::ffi::c_void);
    let icon = crate::daemon::appicon::load_path("C:/Windows/explorer.exe", 64).expect("explorer icon via the shared loader");
    let scale = w.with(|i| i.st.scale);
    let bubble = crate::daemon::pill::model::AppBubble::new(&icon.name, &icon.rgba, icon.size, scale).expect("bubble from loaded icon");
    w.with(|i| {
        i.set_app(Some(bubble));
        i.set_mode(PillMode::Recording);
    });
    pump(900);
    let (g, r) = w.with(|i| (i.st.geometry(i.now()), i.client_size()));
    assert!(g.bubble_t > 0.99, "bubble fully in: {g:?}");
    assert!((g.bubble.2 * 2.0 - 36.0).abs() < 0.01, "diameter == recording pill height");
    // the bubble takes clicks (so it can be dragged and show its tooltip)
    let w_rect = rect(hwnd);
    let (bx, by) = ((g.bubble.0 * scale) as i32, (g.bubble.1 * scale) as i32);
    assert_eq!(send(hwnd, WM_NCHITTEST, 0, lparam(w_rect.left + bx, w_rect.top + by)), HTCLIENT as isize);
    // the icon actually drew non-background pixels inside the circle
    let frame = w.with(|i| crate::daemon::pill::render::render(&i.st, i.now()));
    let (cx, cy) = (bx.clamp(0, r.0 - 1) as u32, by.clamp(0, r.1 - 1) as u32);
    let coloured = (0..11u32)
        .flat_map(|dx| (0..11u32).map(move |dy| (dx, dy)))
        .filter_map(|(dx, dy)| frame.pixel(cx + dx - 5, cy + dy - 5))
        .filter(|p| p.alpha() > 200 && (p.red() as i32 - p.blue() as i32).abs() > 40)
        .count();
    assert!(coloured > 10, "icon colours present at the bubble centre ({coloured})");
    // tooltip on bubble hover names the app
    send(hwnd, MOVE, 0, lparam(bx, by));
    assert!(w.with(|i| i.st.pointer().bubble));
    send(hwnd, WM_MOUSELEAVE, 0, LPARAM(0));
}

// --- glass backdrop (desktop) ------------------------------------------------------------------

use crate::daemon::pill::capture::{Capturer, GlassHost, exclude_from_capture, is_excluded};
use windows::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_NONE};

/// `true` when the desktop lets us copy the screen (a locked session or the secure desktop does not).
fn can_capture() -> bool {
    Capturer::new().is_some_and(|mut c| c.grab(0, 0, 8, 8).is_some())
}

/// Mean absolute channel difference and the share of pixels that differ by more than `tol`.
fn diff(a: &[u8], b: &[u8], tol: u8) -> (f64, f64) {
    let n = a.len() / 4;
    let (mut sum, mut bad) = (0u64, 0usize);
    for (x, y) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        let d = (0..3).map(|c| x[c].abs_diff(y[c])).max().unwrap_or(0);
        sum += (0..3).map(|c| x[c].abs_diff(y[c]) as u64).sum::<u64>();
        bad += (d > tol) as usize;
    }
    (sum as f64 / (n * 3) as f64, bad as f64 / n as f64)
}

/// Pump messages until the worker's answer has been installed (the window handles `WM_APP_GLASS`).
fn wait_glass(w: &Window) {
    let t0 = Instant::now();
    while w.with(|i| i.glass.in_flight) {
        pump(5);
        assert!(t0.elapsed().as_secs() < 10, "the capture never finished");
    }
}

fn hidden_opts() -> PillOptions {
    PillOptions { auto_hide: true, show_app_bubble: false, ..opts() }
}

fn win_screen_rect(w: &Window) -> (i32, i32, i32, i32) {
    w.with(|i| i.win_rect())
}

#[test]
#[ignore]
fn desktop_pill_capture_excludes_the_pill() {
    if !can_capture() {
        println!("SKIPPED: the desktop cannot be captured (locked session?)");
        return;
    }
    // auto-hide: the idle pill is not on screen until a dictation starts
    let w = Window::create(Arc::new(AtomicU32::new(0)), hidden_opts(), Box::new(|_| {})).expect("create pill window");
    let hwnd = HWND(w.hwnd() as *mut core::ffi::c_void);
    assert!(is_excluded(hwnd), "the pill window is excluded from capture");
    // SAFETY: valid handle.
    assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool(), "hidden before the dictation");
    let (l, t, r, b) = win_screen_rect(&w);
    let (cw, ch) = (r - l, b - t);
    let mut cap = Capturer::new().expect("capturer");
    pump(150);
    let before = cap.grab(l, t, cw, ch).expect("grab before").to_vec();
    w.with(|i| i.set_mode(PillMode::Recording));
    pump(1200); // entrance done, capsule fully drawn
    // SAFETY: valid handle.
    assert!(unsafe { IsWindowVisible(hwnd) }.as_bool(), "shown while recording");
    let after = cap.grab(l, t, cw, ch).expect("grab after").to_vec();

    // control: without the exclusion the capsule is in the capture
    // SAFETY: valid handle.
    unsafe {
        SetWindowDisplayAffinity(hwnd, WDA_NONE).expect("clear the affinity");
    }
    pump(400);
    let included = cap.grab(l, t, cw, ch).expect("grab included").to_vec();
    assert!(exclude_from_capture(hwnd) && is_excluded(hwnd), "affinity restored");
    w.with(|i| i.set_mode(PillMode::Idle));

    let (cmean, cbad) = diff(&before, &included, 24);
    println!("control (not excluded): mean abs diff {cmean:.3}, {:.2} % of pixels differ", cbad * 100.0);
    if cbad < 0.05 {
        // a locked session grabs fine but composes no windows: neither comparison can mean anything
        println!("SKIPPED: the control capture does not contain the pill (the desktop is not composed; locked session?)");
        return;
    }
    let (mean, bad) = diff(&before, &after, 24);
    println!("excluded: mean abs diff {mean:.3}, {:.2} % of pixels differ", bad * 100.0);
    assert!(bad < 0.02 && mean < 3.0, "the capture after show equals the capture before (mean {mean:.3}, {:.2} % differ)", bad * 100.0);
}

#[test]
#[ignore]
fn desktop_pill_backdrop_is_captured_only_while_shown_and_hash_gated() {
    if !can_capture() {
        println!("SKIPPED: the desktop cannot be captured (locked session?)");
        return;
    }
    let w = Window::create(Arc::new(AtomicU32::new(0)), hidden_opts(), Box::new(|_| {})).expect("create pill window");
    // hidden: no capture, no timer, no picture kept
    pump(700);
    let (grabs, ms, has) = w.with(|i| (i.glass.grabs, i.glass_ms, i.st.backdrop.is_some()));
    assert_eq!((grabs, ms, has), (0, 0, false), "nothing is captured while hidden");

    // show: captured at once, the timer runs at the active rate while recording
    w.with(|i| i.set_mode(PillMode::Recording));
    wait_glass(&w);
    assert!(w.with(|i| i.glass.grabs) >= 1, "captured on show");
    assert!(w.with(|i| i.st.backdrop.is_some()), "a backdrop is installed");
    pump(300);
    assert_eq!(w.with(|i| i.glass_ms), 125, "~8 Hz while recording");
    let g0 = w.with(|i| (i.glass.grabs, i.glass.unchanged));
    pump(1500);
    let g1 = w.with(|i| (i.glass.grabs, i.glass.unchanged));
    let (grabs, unchanged) = (g1.0 - g0.0, g1.1 - g0.1);
    println!("recording: {grabs} captures in 1.5 s, {unchanged} unchanged");
    assert!((6..=14).contains(&grabs), "at most ~8 Hz, got {grabs} captures in 1.5 s");
    assert!(unchanged * 2 >= grabs, "a static screen finds the hash unchanged ({unchanged}/{grabs})");

    // back to the resting circle: auto-hide hides it again, so the timer is off and the buffers are released
    w.with(|i| i.set_mode(PillMode::Idle));
    pump(1000);
    let (ms, has) = w.with(|i| (i.glass_ms, i.st.backdrop.is_some()));
    assert_eq!((ms, has), (0, false), "hidden again: timer killed, picture dropped");
    let g2 = w.with(|i| i.glass.grabs);
    pump(700);
    assert_eq!(w.with(|i| i.glass.grabs), g2, "no captures while hidden");
}

#[test]
#[ignore]
fn desktop_pill_resting_circle_refreshes_slowly() {
    if !can_capture() {
        println!("SKIPPED: the desktop cannot be captured (locked session?)");
        return;
    }
    let (w, _events) = make(); // auto-hide off: the resting circle stays on screen
    pump(300);
    assert_eq!(w.with(|i| i.glass_ms), 500, "the resting circle refreshes at 2 Hz");
    let g0 = w.with(|i| i.glass.grabs);
    pump(2100);
    let grabs = w.with(|i| i.glass.grabs) - g0;
    println!("resting circle: {grabs} captures in 2.1 s");
    assert!((3..=6).contains(&grabs), "2 Hz, got {grabs}");
}

#[test]
#[ignore]
fn desktop_pill_falls_back_to_neutral_glass_when_capture_is_unavailable() {
    let (w, _events) = make();
    // the host cannot capture (what a refused WDA_EXCLUDEFROMCAPTURE leaves): no picture, never garbage
    w.with(|i| {
        i.glass = GlassHost::unavailable();
        i.glass_stop();
        i.set_mode(PillMode::Recording);
    });
    pump(900);
    let (has, ms, grabs) = w.with(|i| (i.st.backdrop.is_some(), i.glass_ms, i.glass.grabs));
    assert_eq!((has, ms, grabs), (false, 0, 0), "no capture, no timer");
    // the frame on screen is the neutral frosted glass, what the pure renderer draws
    let same = w.with(|i| {
        let now = i.now();
        let want = crate::daemon::pill::render::render(&i.st, now);
        let (a, b) = (i.pm.data(), want.data());
        a.chunks_exact(4).zip(b.chunks_exact(4)).all(|(x, y)| (0..4).all(|c| x[c].abs_diff(y[c]) <= 3))
    });
    assert!(same, "the window shows the neutral glass");
    let mut host = GlassHost::unavailable();
    assert!(!host.request((0, 0, 100, 100), (0, 0, 1920, 1080), 1.0, (1920.0, 1080.0), 0.0));
    assert!(host.take().is_none());
}

#[test]
#[ignore]
fn desktop_pill_backdrop_follows_the_window_when_dragged() {
    if !can_capture() {
        println!("SKIPPED: the desktop cannot be captured (locked session?)");
        return;
    }
    let (w, _events) = make();
    let hwnd = HWND(w.hwnd() as *mut core::ffi::c_void);
    pump(300);
    let (scale, g) = w.with(|i| (i.st.scale, i.st.geometry(i.now())));
    let (px, py) = ((g.pill.cx() * scale) as i32, (g.pill.cy() * scale) as i32);
    let start = rect(hwnd);
    let grabs0 = w.with(|i| i.glass.grabs);
    send(hwnd, WM_LBUTTONDOWN, 1, lparam(px, py));
    let target = (start.left + px - 80, start.top + py - 40);
    for _ in 0..2 {
        let now = rect(hwnd);
        send(hwnd, MOVE, 1, lparam(target.0 - now.left, target.1 - now.top));
    }
    // dragging: the timer runs at the active rate and recaptures the new place within a tick or two
    assert_eq!(w.with(|i| i.glass_ms), 125);
    pump(350);
    assert!(w.with(|i| i.glass.grabs) > grabs0, "recaptured after the move");
    assert!(w.with(|i| i.bd_origin == i.pos), "the picture is placed for the new window origin");
    let now = rect(hwnd);
    send(hwnd, WM_LBUTTONUP, 0, lparam(target.0 - now.left, target.1 - now.top));
    send(hwnd, WM_MOUSELEAVE, 0, LPARAM(0)); // the cursor is elsewhere: no hover keeps the rate up
    pump(700);
    assert_eq!(w.with(|i| i.glass_ms), 500, "back to the resting rate after the drag");
}


#[test]
#[ignore]
fn desktop_pill_capture_cost() {
    use crate::daemon::pill::glass::{Cells, FROST_SIGMA, capture_region, cell_for, margin_logical};
    if !can_capture() {
        println!("SKIPPED: the desktop cannot be captured (locked session?)");
        return;
    }
    let (w, _events) = make();
    pump(200);
    let (win, mon, scale, page) = w.with(|i| (i.win_rect(), i.mon, i.st.scale, i.page()));
    let region = capture_region(win, (margin_logical(page) * scale).ceil() as i32, mon).expect("region");
    let (rw, rh) = (region.2 - region.0, region.3 - region.1);
    let mut cap = Capturer::new().expect("capturer");
    let n = 200;
    let time = |f: &mut dyn FnMut()| {
        f(); // warm
        let t0 = Instant::now();
        for _ in 0..n {
            f();
        }
        t0.elapsed().as_secs_f64() * 1000.0 / n as f64
    };
    let grab = time(&mut || {
        let _ = cap.grab(region.0, region.1, rw, rh).expect("grab").len();
    });
    let raw = cap.grab(region.0, region.1, rw, rh).expect("grab").to_vec();
    let cell = cell_for(scale);
    let cells_ms = time(&mut || {
        let _ = Cells::from_bgra(&raw, rw as usize, rh as usize, cell).expect("cells");
    });
    let cells = Cells::from_bgra(&raw, rw as usize, rh as usize, cell).expect("cells");
    let frost_ms = time(&mut || {
        let _ = cells.frost(FROST_SIGMA, scale);
    });
    println!(
        "capture {rw}x{rh} px ({:.0} KB): BitBlt {grab:.3} ms, cell average + hash {cells_ms:.3} ms (an unchanged screen stops here), frost {frost_ms:.3} ms",
        (rw * rh * 4) as f64 / 1024.0
    );
    // one full round trip through the window's host: request, worker thread, message, install
    let t0 = Instant::now();
    let (sent, ui_ms) = w.with(|i| {
        let (win, page) = (i.win_rect(), i.page());
        let t = Instant::now();
        let sent = i.glass.request(win, mon, scale, page, 1e12);
        (sent, t.elapsed().as_secs_f64() * 1000.0)
    });
    assert!(sent, "a request is accepted");
    wait_glass(&w);
    println!("host round trip: {:.3} ms; the UI thread spent {ui_ms:.3} ms of it (the request only)", t0.elapsed().as_secs_f64() * 1000.0);
}
