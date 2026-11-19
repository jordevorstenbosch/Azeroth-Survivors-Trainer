use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io,
    os::windows::ffi::OsStrExt,
    path::Path,
    process::{Command, Stdio},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW},
    },
};

pub fn valid_path(path: &Path) -> bool {
    path.is_file()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("AzerothSurvivors.exe"))
}

pub fn build_id(path: &Path) -> io::Result<String> {
    let mut hasher = Sha256::new();
    io::copy(&mut File::open(path)?, &mut hasher)?;
    let digest = format!("{:x}", hasher.finalize());
    Ok(digest[..12].to_owned())
}

pub fn running_pid(path: &Path) -> io::Result<Option<u32>> {
    let wanted = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut present = None;
        let mut unreadable = None;
        let mut more = Process32FirstW(snapshot, &mut entry);
        let initial_error = (more == 0).then(io::Error::last_os_error);
        let mut buffer = vec![0u16; 32768];
        while more != 0 {
            let end = entry
                .szExeFile
                .iter()
                .position(|&value| value == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
            if name.eq_ignore_ascii_case("AzerothSurvivors.exe") {
                let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID);
                if handle.is_null() {
                    unreadable = Some(io::Error::last_os_error());
                } else {
                    let mut size = buffer.len() as u32;
                    if QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size) == 0 {
                        unreadable = Some(io::Error::last_os_error());
                    } else {
                        let actual = String::from_utf16_lossy(&buffer[..size as usize]);
                        if actual.eq_ignore_ascii_case(&String::from_utf16_lossy(&wanted)) {
                            present = Some(entry.th32ProcessID);
                        }
                    }
                    CloseHandle(handle);
                    if present.is_some() {
                        break;
                    }
                }
            }
            more = Process32NextW(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
        if present.is_some() {
            Ok(present)
        } else if let Some(error) = initial_error.or(unreadable) {
            Err(error)
        } else {
            Ok(None)
        }
    }
}

pub fn launch(path: &Path) -> io::Result<bool> {
    if !valid_path(path) {
        return Err(io::Error::other(
            "Select a valid AzerothSurvivors.exe first.",
        ));
    }
    if running_pid(path)?.is_some() {
        return Ok(false);
    }
    let folder = path
        .parent()
        .ok_or_else(|| io::Error::other("Game folder unavailable."))?;
    Command::new(path)
        .current_dir(folder)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(true)
}
