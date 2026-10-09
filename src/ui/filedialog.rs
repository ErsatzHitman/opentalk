//! Native open/save dialogs (IFileOpenDialog / IFileSaveDialog) used by dictionary import/export.
//! Both block the calling (UI) thread while open, which is the normal modal-dialog behaviour.
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{FileOpenDialog, FileSaveDialog, IFileDialog, IFileOpenDialog, IFileSaveDialog, IShellItem, SIGDN_FILESYSPATH};
use windows::core::{HSTRING, Interface, PCWSTR, w};

fn path_of(item: &IShellItem) -> Option<String> {
    // SAFETY: GetDisplayName returns a CoTaskMem string that we free after copying it.
    unsafe {
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const _));
        s
    }
}

fn filters(dlg: &IFileDialog) {
    let specs = [
        COMDLG_FILTERSPEC { pszName: w!("Word lists (*.json, *.txt)"), pszSpec: w!("*.json;*.txt") },
        COMDLG_FILTERSPEC { pszName: w!("All files"), pszSpec: w!("*.*") },
    ];
    // SAFETY: `specs` holds static wide strings and outlives the call.
    unsafe {
        let _ = dlg.SetFileTypes(&specs);
    }
}

/// Ask for a file to read. None when cancelled or unavailable.
pub fn open_file() -> Option<String> {
    // SAFETY: plain COM calls on the UI thread; every interface is released on drop.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        filters(&dlg.cast().ok()?);
        dlg.Show(None).ok()?;
        path_of(&dlg.GetResult().ok()?)
    }
}

/// Ask where to save `default_name`. None when cancelled or unavailable.
pub fn save_file(default_name: &str) -> Option<String> {
    // SAFETY: as in `open_file`.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dlg: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        filters(&dlg.cast().ok()?);
        let _ = dlg.SetFileName(&HSTRING::from(default_name));
        let _ = dlg.SetDefaultExtension(PCWSTR(w!("json").0));
        dlg.Show(None).ok()?;
        path_of(&dlg.GetResult().ok()?)
    }
}
