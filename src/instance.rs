use crate::wide;
use std::{
    env,
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::Path,
    process::{Child, Command},
    ptr, thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM,
        WAIT_OBJECT_0, WPARAM,
    },
    System::{
        JobObjects::{
            IsProcessInJob, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            QueryInformationJobObject,
        },
        SystemInformation::GetTickCount64,
        Threading::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, CreateMutexW, GetCurrentProcess,
            OpenMutexW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            QueryFullProcessImageNameW, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
        },
    },
    UI::{
        Input::KeyboardAndMouse::{GetAsyncKeyState, GetLastInputInfo, LASTINPUTINFO, VK_RETURN},
        WindowsAndMessaging::{
            AllowSetForegroundWindow, FindWindowW, GetWindowThreadProcessId, PostMessageW, WM_APP,
            WM_CLOSE,
        },
    },
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
pub const SHOW_MENU: u32 = WM_APP + 2;

/// The Start shortcut's request, valid until the user moves on or startup times out.
#[derive(Clone, Copy)]
pub struct MenuRequest {
    requested_at: u64,
    last_input: u32,
}

impl MenuRequest {
    pub fn capture() -> Option<Self> {
        let mut request = Self {
            requested_at: unsafe { GetTickCount64() },
            last_input: last_input_time()?,
        };
        // Start can launch on Enter down. Its release belongs to that same action.
        let enter_down = || unsafe { GetAsyncKeyState(i32::from(VK_RETURN)) < 0 };
        if enter_down() {
            loop {
                thread::sleep(Duration::from_millis(10));
                if !enter_down() {
                    request.last_input = last_input_time()?;
                    break;
                }
                if !request.is_fresh() || setup_running() {
                    return None;
                }
            }
        }
        Some(request)
    }

    pub fn from_message(time: WPARAM, input: LPARAM) -> Self {
        Self {
            requested_at: time as u64,
            last_input: input as u32,
        }
    }

    pub fn is_fresh(self) -> bool {
        let age = unsafe { GetTickCount64() }.wrapping_sub(self.requested_at);
        age < STARTUP_TIMEOUT.as_millis() as u64 && last_input_time() == Some(self.last_input)
    }

    /// Another copy owns the mutex. It can still be creating its menu window.
    pub fn forward(self) {
        let (hwnd, pid) = loop {
            if !self.is_fresh() || setup_running() {
                return;
            }
            if let Some(owner) = running() {
                break owner;
            }
            thread::sleep(Duration::from_millis(50));
        };
        if let Ok(_owner) = owner_process(hwnd, pid, 0)
            && self.is_fresh()
            && unsafe { AllowSetForegroundWindow(pid) } != 0
        {
            unsafe {
                PostMessageW(
                    hwnd,
                    SHOW_MENU,
                    self.requested_at as WPARAM,
                    self.last_input as LPARAM,
                );
            }
        }
    }
}

fn last_input_time() -> Option<u32> {
    let mut input = LASTINPUTINFO {
        cbSize: size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    (unsafe { GetLastInputInfo(&mut input) } != 0).then_some(input.dwTime)
}

/// Inno controls the installed executable while either of these mutexes exists.
pub fn setup_running() -> bool {
    ["PortsideSetup", "PortsideUninstall"].iter().any(|name| {
        let name = wide(name);
        unsafe {
            let handle = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr());
            if handle.is_null() {
                return GetLastError() == ERROR_ACCESS_DENIED;
            }
            CloseHandle(handle);
            true
        }
    })
}

pub fn acquire() -> Result<Option<OwnedHandle>, String> {
    let name = wide("Portside");
    unsafe {
        let handle = CreateMutexW(ptr::null(), 0, name.as_ptr());
        if handle.is_null() {
            return Err("Could not create Portside's single-instance lock.".into());
        }
        let new = GetLastError() != ERROR_ALREADY_EXISTS;
        Ok(new.then_some(OwnedHandle::from_raw_handle(handle)))
    }
}

/// The running copy's menu window and process, found by class name.
fn running() -> Option<(HWND, u32)> {
    unsafe {
        let hwnd = FindWindowW(wide(crate::ui::CLASS_NAME).as_ptr(), ptr::null());
        if hwnd.is_null() {
            return None;
        }
        let mut pid = 0;
        (GetWindowThreadProcessId(hwnd, &mut pid) != 0).then_some((hwnd, pid))
    }
}

/// Hold the process and recheck its window before granting focus or posting a close.
fn owner_process(hwnd: HWND, pid: u32, access: u32) -> Result<OwnedHandle, String> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | access, 0, pid) };
    if process.is_null() {
        return Err("Could not track the running Portside's shutdown.".into());
    }
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let mut image = [0u16; 32768];
    let mut length = image.len() as u32;
    let named = unsafe {
        QueryFullProcessImageNameW(process.as_raw_handle(), 0, image.as_mut_ptr(), &mut length)
    };
    if named != 0
        && Path::new(&String::from_utf16_lossy(&image[..length as usize]))
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("portside.exe"))
        && running() == Some((hwnd, pid))
    {
        Ok(process)
    } else {
        Err("Could not identify the running Portside.".into())
    }
}

pub fn close_existing() -> Result<(), String> {
    // The installer can retain the lock briefly after its launcher exits.
    // A starting Portside also owns it before creating its window.
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let (hwnd, pid, process) = loop {
        if let Some((hwnd, pid)) = running() {
            match owner_process(hwnd, pid, PROCESS_SYNCHRONIZE) {
                Ok(process) => break (hwnd, pid, process),
                Err(error) if running() == Some((hwnd, pid)) => return Err(error),
                Err(_) => {} // The owner exited while its process was being checked.
            }
        }
        if acquire()?.is_some() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("Portside's single-instance lock is still in use.".into());
        }
        thread::sleep(Duration::from_millis(50));
    };
    unsafe {
        if PostMessageW(hwnd, WM_CLOSE, 0, 0) == 0 && running() == Some((hwnd, pid)) {
            return Err("Could not ask the running Portside to exit.".into());
        }
        // The window disappears before the mutex is released. Wait for the
        // exact owner process, including same-path reinstalls.
        if WaitForSingleObject(process.as_raw_handle(), 5000) != WAIT_OBJECT_0 {
            return Err("The running Portside did not exit. Close it and try again.".into());
        }
    }
    Ok(())
}

pub fn start() -> Result<(), String> {
    let target =
        env::current_exe().map_err(|e| format!("Could not locate this executable: {e}"))?;
    let mut child = spawn_detached(&target)?;
    if let Err(error) = verify_started(&mut child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(())
}

/// Tool hosts (terminals, agent shells) often run their children inside a job
/// object. Portside must outlive whatever launched the installer, so leave
/// that job when the host permits it. When breakaway is forbidden, a plain
/// spawn is fine unless the host's job kills its children on close: then
/// Portside would die with the shell, so refuse to launch it there.
fn spawn_detached(target: &Path) -> Result<Child, String> {
    match Command::new(target)
        .creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
    {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            if job_kills_on_close() {
                return Err("This shell keeps its child processes in a job that closes them with the shell and forbids breakaway. Run Portside from a regular terminal.".into());
            }
            Command::new(target).creation_flags(CREATE_NO_WINDOW).spawn()
        }
        result => result,
    }
    .map_err(|e| format!("Installed, but could not start Portside: {e}"))
}

/// Whether the installer's own job has JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE set.
fn job_kills_on_close() -> bool {
    unsafe {
        let mut in_job = 0;
        if IsProcessInJob(GetCurrentProcess(), ptr::null_mut(), &mut in_job) == 0 || in_job == 0 {
            return false;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        QueryInformationJobObject(
            ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut info).cast(),
            size_of_val(&info) as u32,
            ptr::null_mut(),
        ) != 0
            && info.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0
    }
}

fn verify_started(child: &mut Child) -> Result<(), String> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut window_since = None;
    loop {
        if child
            .try_wait()
            .map_err(|e| format!("Could not verify the installed Portside: {e}"))?
            .is_some()
        {
            return Err("The installed Portside stopped before startup could be verified. Another copy may still be running.".into());
        }
        if running().is_some_and(|(_, pid)| pid == child.id()) {
            let since = window_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(500) {
                return Ok(());
            }
        } else {
            window_since = None;
        }
        if Instant::now() >= deadline {
            return Err("The installed Portside did not create its window.".into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}
