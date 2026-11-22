use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, RECT};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowRect, GetWindowTextLengthW, GetWindowThreadProcessId,
    IsWindowVisible,
};

use crate::{portable_command, Result};

pub fn launch_command(exe: &Path) -> Result<Command> {
    let mut command = Command::new(exe);
    portable_command::configure(&mut command, exe, true)?;
    command.env("CODEX_SPARKLE_ENABLED", "false");
    command.env_remove("CODEX_WINDOWS_SANDBOX_PACKAGE_FAMILY");
    Ok(command)
}

struct HealthJob(HANDLE);

impl HealthJob {
    fn create() -> io::Result<Self> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(handle);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let success = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if success == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }
}

impl Drop for HealthJob {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct Probe {
    pid: u32,
    found: bool,
}

unsafe extern "system" fn match_window(window: HWND, context: LPARAM) -> i32 {
    let probe = unsafe { &mut *(context as *mut Probe) };
    let mut owner = 0;
    unsafe {
        GetWindowThreadProcessId(window, &mut owner);
    }
    if owner != probe.pid || unsafe { IsWindowVisible(window) } == 0 {
        return 1;
    }
    let mut class = [0u16; 64];
    let length = unsafe { GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) };
    if length <= 0
        || !String::from_utf16_lossy(&class[..length as usize]).starts_with("Chrome_WidgetWin_")
    {
        return 1;
    }
    let mut dimensions = RECT::default();
    if unsafe { GetWindowRect(window, &mut dimensions) } != 0
        && unsafe { GetWindowTextLengthW(window) } > 0
        && dimensions.right - dimensions.left >= 350
        && dimensions.bottom - dimensions.top >= 220
    {
        probe.found = true;
        return 0;
    }
    1
}

fn visible_window(pid: u32) -> bool {
    let mut probe = Probe { pid, found: false };
    unsafe {
        EnumWindows(Some(match_window), (&mut probe as *mut Probe) as LPARAM);
    }
    probe.found
}

pub fn check(exe: &Path, storage: &Path) -> Result<()> {
    let profile = tempfile::Builder::new()
        .prefix("health-")
        .tempdir_in(storage)?;
    let electron = profile.path().join("desktop");
    let codex = profile.path().join("codex");
    std::fs::create_dir(&electron)?;
    std::fs::create_dir(&codex)?;
    let job = HealthJob::create()?;
    let mut user_data_arg = std::ffi::OsString::from("--user-data-dir=");
    user_data_arg.push(&electron);
    let mut child = launch_command(exe)?
        .arg(user_data_arg)
        .env("CODEX_ELECTRON_USER_DATA_PATH", &electron)
        .env("CODEX_HOME", &codex)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle().cast()) } == 0 {
        let error = io::Error::last_os_error();
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("Could not isolate the health-check process: {error}").into());
    }
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut visible_since = None;
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break Err(format!("ChatGPT exited during health check ({status})").into())
            }
            Err(error) => break Err(error.into()),
            Ok(None) => {}
        }
        if visible_window(child.id()) {
            if visible_since.get_or_insert_with(Instant::now).elapsed() >= Duration::from_secs(5) {
                break Ok(());
            }
        } else {
            visible_since = None;
        }
        if Instant::now() >= deadline {
            break Err("ChatGPT did not show a stable main window within 90 seconds".into());
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    drop(job);
    let _ = child.wait();
    result
}
