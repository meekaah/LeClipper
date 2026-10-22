//! Ties the settings and popup child processes to the tray process, and lets the tray see
//! whether any popup is in the middle of an Imgur upload.

use std::os::windows::io::AsRawHandle;
use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::*;
use windows::Win32::System::Threading::{CreateMutexW, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};

const UPLOADING: PCWSTR = w!("Local\\LeClipper-uploading");

struct Job(HANDLE);
// The handle is only used for AssignProcessToJobObject, which is thread-safe.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

static JOB: OnceLock<Option<Job>> = OnceLock::new();

/// A job that kills its processes when the last handle to it closes, i.e. when the tray
/// process exits (normally or not). Grandchildren (browsers, Explorer) break away from it.
fn job() -> Option<&'static Job> {
    JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(None, PCWSTR::null()).ok()?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of_val(&info) as u32,
        );
        if set.is_err() {
            let _ = CloseHandle(job);
            return None;
        }
        Some(Job(job))
    })
    .as_ref()
}

/// Makes `child` exit together with this process.
pub fn adopt(child: &std::process::Child) {
    let Some(job) = job() else { return };
    let res = unsafe { AssignProcessToJobObject(job.0, HANDLE(child.as_raw_handle())) };
    if let Err(e) = res {
        crate::log(&format!("couldn't add child process to job: {e}"));
    }
}

/// Held by a popup while it uploads; any number of popups can hold one at once.
pub struct UploadMarker(HANDLE);

impl UploadMarker {
    pub fn new() -> Option<Self> {
        unsafe { CreateMutexW(None, false, UPLOADING).ok().map(Self) }
    }
}

impl Drop for UploadMarker {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// True while any popup holds an [`UploadMarker`].
pub fn upload_in_progress() -> bool {
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, UPLOADING) } {
        Ok(h) => {
            unsafe {
                let _ = CloseHandle(h);
            }
            true
        }
        Err(_) => false,
    }
}
