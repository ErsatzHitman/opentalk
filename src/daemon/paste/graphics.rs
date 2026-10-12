//! Serialize clipboard GDI objects into bounded, owned bytes. No snapshot retains an owner's
//! handles: restore creates fresh objects, whose ownership transfers to the clipboard on success.
use super::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::METAFILEPICT;

pub(super) fn supported(fmt: u32) -> bool {
    matches!(fmt, CF_BITMAP | CF_DSPBITMAP | CF_ENHMETAFILE | CF_DSPENHMETAFILE | CF_METAFILEPICT | CF_DSPMETAFILEPICT | CF_PALETTE)
}

fn bitmap_info(h: HANDLE) -> Option<(BITMAPINFO, usize)> {
    // SAFETY: h is a clipboard bitmap; GetObject writes into a correctly sized BITMAP.
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(HGDIOBJ(h.0), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as _)) == 0 { return None; }
        let width = usize::try_from(bm.bmWidth).ok()?;
        let height = usize::try_from(bm.bmHeight).ok()?;
        let size = width.checked_mul(height)?.checked_mul(4)?.checked_add(40)?;
        let info = BITMAPINFO { bmiHeader: BITMAPINFOHEADER {
            biSize: 40, biWidth: bm.bmWidth, biHeight: bm.bmHeight, biPlanes: 1, biBitCount: 32,
            biCompression: BI_RGB.0, ..Default::default()
        }, ..Default::default() };
        Some((info, size))
    }
}

fn metafile(h: HANDLE) -> Option<METAFILEPICT> {
    // SAFETY: CF_METAFILEPICT is an HGLOBAL containing this structure; lock only to copy it.
    unsafe {
        let hg = HGLOBAL(h.0);
        if GlobalSize(hg) < std::mem::size_of::<METAFILEPICT>() { return None; }
        let p = GlobalLock(hg);
        if p.is_null() { return None; }
        let value = std::ptr::read_unaligned(p as *const METAFILEPICT);
        let _ = GlobalUnlock(hg);
        Some(value)
    }
}

pub(super) fn size(fmt: u32, h: HANDLE) -> Option<usize> {
    // SAFETY: handles have the type mandated by fmt; size calls do not take ownership.
    unsafe {
        match fmt {
            CF_BITMAP | CF_DSPBITMAP => bitmap_info(h).map(|(_, n)| n),
            CF_ENHMETAFILE | CF_DSPENHMETAFILE => {
                let n = GetEnhMetaFileBits(HENHMETAFILE(h.0), None);
                (n > 0).then_some(n as usize)
            }
            CF_METAFILEPICT | CF_DSPMETAFILEPICT => {
                let n = GetMetaFileBitsEx(metafile(h)?.hMF, 0, None);
                (n > 0).then_some(n as usize + 12)
            }
            CF_PALETTE => {
                let n = GetPaletteEntries(HPALETTE(h.0), 0, None);
                (n > 0 && n <= u16::MAX as u32).then_some(4 + n as usize * 4)
            }
            _ => None,
        }
    }
}

pub(super) fn read(fmt: u32, h: HANDLE, size: usize) -> Option<Vec<u8>> {
    let mut bytes = vec![0u8; size];
    // SAFETY: the caller measured and bounded size while holding the clipboard open. All output
    // slices match that measurement; local DCs are released before returning.
    unsafe {
        match fmt {
            CF_BITMAP | CF_DSPBITMAP => {
                let (mut info, expected) = bitmap_info(h)?;
                if expected != size { return None; }
                let dc = GetDC(None);
                if dc.is_invalid() { return None; }
                let rows = GetDIBits(dc, HBITMAP(h.0), 0, info.bmiHeader.biHeight as u32, Some(bytes[40..].as_mut_ptr() as _), &mut info, DIB_RGB_COLORS);
                let _ = ReleaseDC(None, dc);
                if rows != info.bmiHeader.biHeight { return None; }
                std::ptr::copy_nonoverlapping(&info.bmiHeader as *const _ as *const u8, bytes.as_mut_ptr(), 40);
            }
            CF_ENHMETAFILE | CF_DSPENHMETAFILE => {
                if GetEnhMetaFileBits(HENHMETAFILE(h.0), Some(&mut bytes)) as usize != size { return None; }
            }
            CF_METAFILEPICT | CF_DSPMETAFILEPICT => {
                let m = metafile(h)?;
                bytes[..4].copy_from_slice(&m.mm.to_le_bytes());
                bytes[4..8].copy_from_slice(&m.xExt.to_le_bytes());
                bytes[8..12].copy_from_slice(&m.yExt.to_le_bytes());
                if GetMetaFileBitsEx(m.hMF, (size - 12) as u32, Some(bytes[12..].as_mut_ptr() as _)) as usize != size - 12 { return None; }
            }
            CF_PALETTE => {
                let n = (size - 4) / 4;
                let mut entries = vec![PALETTEENTRY::default(); n];
                if GetPaletteEntries(HPALETTE(h.0), 0, Some(&mut entries)) as usize != n { return None; }
                bytes[..2].copy_from_slice(&0x300u16.to_le_bytes());
                bytes[2..4].copy_from_slice(&(n as u16).to_le_bytes());
                std::ptr::copy_nonoverlapping(entries.as_ptr() as *const u8, bytes[4..].as_mut_ptr(), n * 4);
            }
            _ => return None,
        }
    }
    Some(bytes)
}

pub(super) fn set(fmt: u32, bytes: &[u8]) -> bool {
    // SAFETY: bytes are our own serialized snapshot. Handles transfer on SetClipboardData success;
    // every failure releases the newly created object, never a source clipboard handle.
    unsafe {
        match fmt {
            CF_BITMAP | CF_DSPBITMAP => {
                if bytes.len() < 40 { return false; }
                let header = std::ptr::read_unaligned(bytes.as_ptr() as *const BITMAPINFOHEADER);
                let info = BITMAPINFO { bmiHeader: header, ..Default::default() };
                let dc = GetDC(None);
                if dc.is_invalid() { return false; }
                let h = CreateDIBitmap(dc, Some(&header), CBM_INIT as u32, Some(bytes[40..].as_ptr() as _), Some(&info), DIB_RGB_COLORS);
                let _ = ReleaseDC(None, dc);
                if h.is_invalid() { return false; }
                if SetClipboardData(fmt, Some(HANDLE(h.0))).is_ok() { return true; }
                let _ = DeleteObject(HGDIOBJ(h.0));
            }
            CF_ENHMETAFILE | CF_DSPENHMETAFILE => {
                let h = SetEnhMetaFileBits(bytes);
                if h.is_invalid() { return false; }
                if SetClipboardData(fmt, Some(HANDLE(h.0))).is_ok() { return true; }
                let _ = DeleteEnhMetaFile(Some(h));
            }
            CF_METAFILEPICT | CF_DSPMETAFILEPICT => {
                if bytes.len() < 12 { return false; }
                let h = SetMetaFileBitsEx(&bytes[12..]);
                if h.is_invalid() { return false; }
                let Ok(hg) = GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of::<METAFILEPICT>()) else { let _ = DeleteMetaFile(h); return false; };
                let p = GlobalLock(hg);
                if !p.is_null() {
                    let m = METAFILEPICT { mm: i32::from_le_bytes(bytes[..4].try_into().unwrap()), xExt: i32::from_le_bytes(bytes[4..8].try_into().unwrap()), yExt: i32::from_le_bytes(bytes[8..12].try_into().unwrap()), hMF: h };
                    std::ptr::write(p as *mut METAFILEPICT, m);
                    let _ = GlobalUnlock(hg);
                    if SetClipboardData(fmt, Some(HANDLE(hg.0))).is_ok() { return true; }
                }
                let _ = GlobalFree(Some(hg));
                let _ = DeleteMetaFile(h);
            }
            CF_PALETTE => {
                if bytes.len() < 4 { return false; }
                let mut aligned = vec![0u32; bytes.len().div_ceil(4)];
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), aligned.as_mut_ptr() as *mut u8, bytes.len());
                let h = CreatePalette(aligned.as_ptr() as *const LOGPALETTE);
                if h.is_invalid() { return false; }
                if SetClipboardData(fmt, Some(HANDLE(h.0))).is_ok() { return true; }
                let _ = DeleteObject(HGDIOBJ(h.0));
            }
            _ => {}
        }
    }
    false
}
