//! App icons for History rows: the app's own Windows icon, resolved once per app on the popup thread
//! and kept (32 px, 4 KB each). Only the cheap extraction paths are used ([`appicon::load_cheap`]).
//! An entry recorded before paths were saved is resolved by exe name ([`appicon::find_exe`]).
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::daemon::appicon;

/// Source size of a row icon (drawn at 20 logical px, so sharp up to 150% scaling).
pub const PX: u32 = 32;

/// Premultiplied RGBA, `size` x `size`.
pub struct AppPix {
    pub size: u32,
    pub rgba: Vec<u8>,
}

thread_local! {
    static ICONS: RefCell<HashMap<String, Option<Rc<AppPix>>>> = RefCell::new(HashMap::new());
}

fn key_of(path: Option<&str>, exe: Option<&str>) -> Option<String> {
    let name = exe.filter(|e| !e.is_empty()).map(str::to_string).or_else(|| path.and_then(|p| p.rsplit(['\\', '/']).next()).map(str::to_string))?;
    Some(name.to_ascii_lowercase())
}

/// The app's icon, or `None` (draw a monogram). Loads at most once per app.
pub fn get(path: Option<&str>, exe: Option<&str>) -> Option<Rc<AppPix>> {
    let key = key_of(path, exe)?;
    ICONS.with(|m| {
        if let Some(hit) = m.borrow().get(&key) {
            return hit.clone();
        }
        let found;
        let path = match path.filter(|p| !p.is_empty()) {
            Some(p) => p,
            None => {
                found = appicon::find_exe(&key);
                match found.as_deref() {
                    Some(p) => p,
                    None => {
                        m.borrow_mut().insert(key, None);
                        return None;
                    }
                }
            }
        };
        let pix = appicon::load_cheap(path, PX).map(|ic| {
            let mut rgba = ic.rgba;
            for px in rgba.chunks_exact_mut(4) {
                let a = px[3] as u32;
                for c in &mut px[..3] {
                    *c = ((*c as u32 * a + 127) / 255) as u8;
                }
            }
            Rc::new(AppPix { size: ic.size, rgba })
        });
        m.borrow_mut().insert(key, pix.clone());
        pix
    })
}
