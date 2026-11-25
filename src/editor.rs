//! Opens the selected project's folder in the installed Windows VS Code.

use crate::{branches, wide};
use std::{
    env,
    ffi::OsString,
    os::windows::{ffi::OsStringExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::ERROR_SUCCESS,
    System::{
        Registry::{HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW},
        Threading::CREATE_NO_WINDOW,
    },
};

// Git lists these repository-local overrides with "git rev-parse --local-env-vars".
const REPOSITORY_ENV: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_DIR",
    "GIT_GRAFT_FILE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
    "GIT_REPLACE_REF_BASE",
    "GIT_SHALLOW_FILE",
    "GIT_WORK_TREE",
];

fn registered(root: HKEY) -> Option<PathBuf> {
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\App Paths\Code.exe");
    // Allow the full length of a Windows path, including its terminating NUL.
    let mut path = vec![0u16; 32_768];
    let mut size = (path.len() * size_of::<u16>()) as u32;
    let result = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            null(),
            RRF_RT_REG_SZ,
            null_mut(),
            path.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if result != ERROR_SUCCESS {
        return None;
    }
    let units = path
        .get(..size as usize / size_of::<u16>())?
        .strip_suffix(&[0])?;
    Some(OsString::from_wide(units).into())
}

fn is_editor(exe: &Path) -> bool {
    exe.is_absolute()
        && exe
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("Code.exe"))
        && exe.is_file()
}

pub fn discover() -> Option<PathBuf> {
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        if let Some(exe) = registered(root).filter(|exe| is_editor(exe)) {
            return Some(exe);
        }
    }
    let search_path = env::var_os("PATH")?;
    env::split_paths(&search_path).find_map(|bin| {
        // The official bin\code.cmd runs Code.exe from its parent directory.
        if !bin.is_absolute()
            || !bin
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("bin"))
            || !bin.join("code.cmd").is_file()
        {
            return None;
        }
        let exe = bin.parent()?.join("Code.exe");
        is_editor(&exe).then_some(exe)
    })
}

pub fn available(dir: &Path) -> bool {
    dir.is_absolute() && dir.read_dir().is_ok()
}

/// Git can fail for a usable folder. A directory deleted during the lookup
/// must not be passed to VS Code, which could open it as a new file.
fn folder(dir: &Path) -> Option<PathBuf> {
    if !dir.is_absolute() {
        return None;
    }
    let root = branches::worktree_root(dir);
    if !available(dir) {
        return None;
    }
    match root {
        Some(root) if root == dir || available(&root) => Some(root),
        _ => Some(dir.to_owned()),
    }
}

/// Called on a worker. The editor gets one absolute directory argument.
pub fn open(exe: &Path, dir: &Path) -> bool {
    if !is_editor(exe) {
        return false;
    }
    let Some(folder) = folder(dir) else {
        return false;
    };
    let mut command = Command::new(exe);
    // Let Git discover the clicked checkout. VS Code's extension host also
    // drops empty config values, so discard the counted entries together.
    for (key, _) in env::vars_os() {
        let name = key.to_string_lossy().to_ascii_uppercase();
        if REPOSITORY_ENV.contains(&name.as_str())
            || name.starts_with("GIT_CONFIG_KEY_")
            || name.starts_with("GIT_CONFIG_VALUE_")
        {
            command.env_remove(key);
        }
    }
    command
        .arg(folder)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::SystemTime};

    #[test]
    fn uses_a_plain_folder_and_ignores_unavailable_launch_targets() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp = env::temp_dir().join(format!("portside-editor-{}-{unique}", std::process::id()));
        let project = temp.join("menus & café");
        fs::create_dir_all(&project).unwrap();
        let exe = temp.join("Code.exe");
        let missing_dir = temp.join("removed");

        let plain = folder(&project);
        let missing = folder(&missing_dir);
        let missing_exe_launched = open(&exe, &project);
        fs::write(&exe, b"Invalid executable").unwrap();
        let invalid_exe_launched = open(&exe, &project);
        fs::remove_dir_all(&temp).unwrap();

        assert_eq!(plain, Some(project));
        assert_eq!(missing, None);
        assert!(!available(Path::new(".")));
        assert!(!missing_exe_launched);
        assert!(!invalid_exe_launched);
    }
}
