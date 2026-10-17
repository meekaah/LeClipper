//! Drags a file out of our window exactly like Explorer does: the shell builds the data
//! object (CF_HDROP and friends) and runs the OLE drag loop, so browsers, Discord,
//! Explorer etc. all accept the drop.

use std::path::Path;

use anyhow::{Context, Result};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::Ole::{OleInitialize, DROPEFFECT_COPY};
use windows::Win32::UI::Shell::{BHID_DataObject, IShellItem, SHCreateItemFromParsingName, SHDoDragDrop};

/// Starts a drag of `path` and blocks until it's dropped or cancelled.
/// Must be called on the UI thread while the left mouse button is down.
pub fn drag_file(path: &Path) -> Result<()> {
    let path = std::path::absolute(path)?;
    unsafe {
        // winit normally has OLE up already; this is a no-op then.
        let _ = OleInitialize(None);
        let wide = HSTRING::from(path.as_os_str());
        let item: IShellItem =
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).context("shell item for the file")?;
        let data: IDataObject = item.BindToHandler(None, &BHID_DataObject).context("shell data object")?;
        // No custom drop source: the shell supplies one, plus the drag image.
        SHDoDragDrop(None, &data, None, DROPEFFECT_COPY).context("drag and drop")?;
    }
    Ok(())
}
