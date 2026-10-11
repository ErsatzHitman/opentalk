//! Rename an imported DLL in a PE image, in place.
//!
//! Windows 11 ships an older `onnxruntime.dll` in System32, and on some machines the loader picks it
//! over the one next to the sherpa-onnx executables ("requested API version is not available";
//! OpenWhispr #2054). The runtime is therefore installed under a private name and every executable's
//! import table is rewritten to it. The new name must not be longer than the old one, because the
//! string is overwritten where it is (the rest is zero-filled).

const IMPORT_DIR: usize = 1;
const DELAY_IMPORT_DIR: usize = 13;

struct Section {
    va: u32,
    vsize: u32,
    raw_size: u32,
    raw_ptr: u32,
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

fn rva_to_offset(rva: u32, sections: &[Section]) -> Option<usize> {
    sections.iter().find_map(|s| {
        let span = s.vsize.max(s.raw_size);
        (rva >= s.va && rva < s.va.checked_add(span)?).then(|| (rva - s.va + s.raw_ptr) as usize)
    })
}

/// File offsets of every module name referenced by the import and delay-load directories.
fn module_name_offsets(img: &[u8]) -> Result<Vec<usize>, String> {
    if img.get(0..2) != Some(b"MZ") {
        return Err("not a PE image (no MZ header)".into());
    }
    let pe = u32_at(img, 0x3c).ok_or("truncated header")? as usize;
    if img.get(pe..pe + 4) != Some(b"PE\0\0") {
        return Err("not a PE image (no PE signature)".into());
    }
    let n_sections = u16_at(img, pe + 6).ok_or("truncated header")? as usize;
    let opt_size = u16_at(img, pe + 20).ok_or("truncated header")? as usize;
    let opt = pe + 24;
    let (n_dirs_at, dirs_at) = match u16_at(img, opt).ok_or("truncated header")? {
        0x10b => (opt + 92, opt + 96),
        0x20b => (opt + 108, opt + 112),
        m => return Err(format!("unknown optional header 0x{m:x}")),
    };
    let n_dirs = u32_at(img, n_dirs_at).ok_or("truncated header")? as usize;
    let sections: Vec<Section> = (0..n_sections)
        .map(|i| {
            let h = opt + opt_size + i * 40;
            Some(Section { vsize: u32_at(img, h + 8)?, va: u32_at(img, h + 12)?, raw_size: u32_at(img, h + 16)?, raw_ptr: u32_at(img, h + 20)? })
        })
        .collect::<Option<_>>()
        .ok_or("truncated section table")?;
    let dir = |i: usize| -> Option<u32> { (i < n_dirs).then(|| u32_at(img, dirs_at + i * 8)).flatten().filter(|&va| va != 0) };

    let mut names = Vec::new();
    if let Some(va) = dir(IMPORT_DIR) {
        let mut d = rva_to_offset(va, &sections).ok_or("import directory outside the sections")?;
        loop {
            let (oft, name, ft) = (u32_at(img, d).ok_or("truncated imports")?, u32_at(img, d + 12).ok_or("truncated imports")?, u32_at(img, d + 16).ok_or("truncated imports")?);
            if oft == 0 && name == 0 && ft == 0 {
                break;
            }
            names.push(rva_to_offset(name, &sections).ok_or("import name outside the sections")?);
            d += 20;
        }
    }
    if let Some(va) = dir(DELAY_IMPORT_DIR) {
        let mut d = rva_to_offset(va, &sections).ok_or("delay-import directory outside the sections")?;
        loop {
            let (attrs, name) = (u32_at(img, d).ok_or("truncated delay imports")?, u32_at(img, d + 4).ok_or("truncated delay imports")?);
            if attrs == 0 && name == 0 {
                break;
            }
            if attrs & 1 == 0 {
                return Err("old-style delay-import descriptor".into());
            }
            names.push(rva_to_offset(name, &sections).ok_or("delay-import name outside the sections")?);
            d += 32;
        }
    }
    Ok(names)
}

fn name_at(img: &[u8], off: usize) -> Option<&[u8]> {
    let rest = img.get(off..)?;
    let end = rest.iter().take(256).position(|&c| c == 0)?;
    Some(&rest[..end])
}

/// The DLL names `img` imports.
#[cfg_attr(not(test), allow(dead_code))]
pub fn imports(img: &[u8]) -> Result<Vec<String>, String> {
    Ok(module_name_offsets(img)?.into_iter().filter_map(|o| name_at(img, o)).map(|n| String::from_utf8_lossy(n).into_owned()).collect())
}

/// Point every import of `from` (case-insensitive) at `to`. Returns how many were rewritten.
pub fn rename_import(img: &mut [u8], from: &str, to: &str) -> Result<usize, String> {
    if to.len() > from.len() || !to.is_ascii() {
        return Err(format!("{to} cannot replace {from} in place"));
    }
    let mut n = 0;
    for off in module_name_offsets(img)? {
        let Some(name) = name_at(img, off) else { continue };
        if name.eq_ignore_ascii_case(from.as_bytes()) {
            let len = name.len();
            let slot = &mut img[off..off + len];
            slot.fill(0);
            slot[..to.len()].copy_from_slice(to.as_bytes());
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test binary itself is a PE image with imports.
    fn own_image() -> Vec<u8> {
        std::fs::read(std::env::current_exe().unwrap()).unwrap()
    }

    #[test]
    fn reads_and_renames_the_imports_of_a_real_image() {
        let mut img = own_image();
        let before = imports(&img).unwrap();
        let k32 = before.iter().find(|n| n.eq_ignore_ascii_case("kernel32.dll")).expect("every Windows image imports kernel32").clone();
        let n = before.iter().filter(|n| n.eq_ignore_ascii_case("kernel32.dll")).count();
        // MinGW images can import one DLL through several descriptors: every one is renamed
        assert_eq!(rename_import(&mut img, "KERNEL32.DLL", "kernel33.dll").unwrap(), n);
        let after = imports(&img).unwrap();
        assert!(after.iter().any(|n| n == "kernel33.dll"), "{after:?}");
        assert!(!after.contains(&k32));
        assert_eq!(after.len(), before.len(), "nothing else moved");
        // shorter names are zero-padded and still read back cleanly
        assert_eq!(rename_import(&mut img, "kernel33.dll", "k.dll").unwrap(), n);
        assert!(imports(&img).unwrap().iter().any(|n| n == "k.dll"));
    }

    #[test]
    fn refuses_longer_names_and_non_pe_input() {
        let mut img = own_image();
        assert!(rename_import(&mut img, "kernel32.dll", "a-much-longer-name.dll").is_err());
        assert!(imports(b"not a pe").is_err());
        assert!(imports(&[b'M', b'Z', 0, 0]).is_err());
        let mut junk = vec![0u8; 512];
        junk[0..2].copy_from_slice(b"MZ");
        junk[0x3c] = 0x80;
        assert!(imports(&junk).is_err(), "no PE signature");
    }

    #[test]
    fn no_match_changes_nothing() {
        let mut img = own_image();
        let copy = img.clone();
        assert_eq!(rename_import(&mut img, "onnxruntime.dll", "ot-onnxrt.dll").unwrap(), 0);
        assert_eq!(img, copy);
    }
}
