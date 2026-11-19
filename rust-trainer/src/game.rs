use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io,
    path::Path,
    process::{Command, Stdio},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_NO_MORE_FILES, FILETIME, INVALID_HANDLE_VALUE},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessInstance {
    pub pid: u32,
    pub created: Option<u64>,
}

pub fn running_instance(path: &Path) -> io::Result<Option<ProcessInstance>> {
    find_process(path, true)
}

pub fn running_pid(path: &Path) -> io::Result<Option<u32>> {
    Ok(legacy_instance(path)?.map(|process| process.pid))
}

pub fn legacy_instance(path: &Path) -> io::Result<Option<ProcessInstance>> {
    find_process(path, false)
}

fn find_process(path: &Path, require_creation_time: bool) -> io::Result<Option<ProcessInstance>> {
    let wanted = path.to_string_lossy();
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut present = None;
        let mut unreadable = None;
        let mut matching_paths = 0;
        let mut more = Process32FirstW(snapshot, &mut entry);
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
                        if actual.eq_ignore_ascii_case(&wanted) {
                            matching_paths += 1;
                            let pid = entry.th32ProcessID;
                            if require_creation_time {
                                let mut created = FILETIME::default();
                                let mut exited = FILETIME::default();
                                let mut kernel = FILETIME::default();
                                let mut user = FILETIME::default();
                                if GetProcessTimes(
                                    handle,
                                    &mut created,
                                    &mut exited,
                                    &mut kernel,
                                    &mut user,
                                ) == 0
                                {
                                    unreadable = Some(io::Error::last_os_error());
                                } else {
                                    let created = (u64::from(created.dwHighDateTime) << 32)
                                        | u64::from(created.dwLowDateTime);
                                    present = Some(ProcessInstance {
                                        pid,
                                        created: Some(created),
                                    });
                                }
                            } else {
                                present = Some(ProcessInstance { pid, created: None });
                            }
                        }
                    }
                    CloseHandle(handle);
                }
            }
            more = Process32NextW(snapshot, &mut entry);
        }
        let enumeration_end = io::Error::last_os_error();
        CloseHandle(snapshot);
        if enumeration_end.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
            Err(enumeration_end)
        } else if matching_paths > 1 {
            Err(io::Error::other(
                "Multiple game processes match the configured path.",
            ))
        } else if present.is_some() {
            Ok(present)
        } else if let Some(error) = unreadable {
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
