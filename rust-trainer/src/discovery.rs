use crate::game;
use std::{
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, HANDLE},
    System::{
        Diagnostics::Debug::ReadProcessMemory,
        Memory::{
            MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_READWRITE, PAGE_WRITECOPY,
            VirtualQueryEx,
        },
        Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ},
    },
};

const CHUNK: usize = 512 * 1024;
const NAME_REACH: usize = 1536;
const STAT_BYTES: usize = 32;
const LEVEL_DISTANCE: usize = 104;

#[derive(Debug, PartialEq, Eq)]
pub struct Report {
    pub build: String,
    pub candidates: usize,
    pub unreadable_chunks: usize,
}

pub struct Job {
    cancel: Arc<AtomicBool>,
    pub result: Receiver<io::Result<Report>>,
}

impl Job {
    pub fn start(path: PathBuf, instance: game::ProcessInstance) -> io::Result<Self> {
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, result) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("stat-discovery".into())
            .spawn(move || {
                let report = (|| {
                    let memory = Memory::open(instance)?;
                    let build = game::build_id(&path)?;
                    check_cancel(&worker_cancel)?;
                    memory.scan(build, &worker_cancel)
                })();
                let _ = sender.send(report);
            })?;
        Ok(Self { cancel, result })
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

struct Memory(HANDLE);

impl Memory {
    fn open(instance: game::ProcessInstance) -> io::Result<Self> {
        let handle =
            unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, instance.pid) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let memory = Self(handle);
        if instance.created != Some(game::creation_time(handle)?) {
            return Err(io::Error::other("Game process changed before discovery."));
        }
        Ok(memory)
    }

    fn read(&self, address: usize, bytes: &mut [u8]) -> io::Result<()> {
        let mut read = 0;
        let ok = unsafe {
            ReadProcessMemory(
                self.0,
                address as _,
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                &mut read,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if read != bytes.len() {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        Ok(())
    }

    fn scan(&self, build: String, cancel: &AtomicBool) -> io::Result<Report> {
        let mut report = Report {
            build,
            candidates: 0,
            unreadable_chunks: 0,
        };
        let mut next = 0;
        let mut buffer = vec![0; CHUNK + NAME_REACH + LEVEL_DISTANCE + 4];
        loop {
            check_cancel(cancel)?;
            let mut region = MEMORY_BASIC_INFORMATION::default();
            let size = unsafe {
                VirtualQueryEx(
                    self.0,
                    next as _,
                    &mut region,
                    size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            if size == 0 {
                let error = io::Error::last_os_error();
                // ERROR_INVALID_PARAMETER marks the end of user-mode addresses.
                return if next != 0 && error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32)
                {
                    Ok(report)
                } else {
                    Err(error)
                };
            }
            let start = region.BaseAddress as usize;
            let end = start
                .checked_add(region.RegionSize)
                .filter(|end| *end > next)
                .ok_or_else(|| io::Error::other("Invalid memory region bounds."))?;
            next = end;
            if region.State != MEM_COMMIT
                || region.Protect & PAGE_GUARD != 0
                || !matches!(region.Protect & 0xff, PAGE_READWRITE | PAGE_WRITECOPY)
            {
                continue;
            }
            for cursor in (start..end).step_by(CHUNK) {
                check_cancel(cancel)?;
                let from = cursor.saturating_sub(NAME_REACH).max(start);
                let last = cursor.saturating_add(CHUNK).min(end);
                let to = last.saturating_add(LEVEL_DISTANCE + 4).min(end);
                let bytes = &mut buffer[..to - from];
                if self.read(from, bytes).is_err() {
                    report.unreadable_chunks += 1;
                    continue;
                }
                for offset in candidate_offsets(bytes, cursor - from, last - from) {
                    check_cancel(cancel)?;
                    if name_headers(&bytes[..offset]).any(|(pointer, length)| {
                        let mut name = [0; 24];
                        let name = &mut name[..length];
                        self.read(pointer, name).is_ok() && plausible_name(name)
                    }) {
                        report.candidates += 1;
                    }
                }
            }
        }
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn check_cancel(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::from(io::ErrorKind::Interrupted))
    } else {
        Ok(())
    }
}

fn candidate_offsets(bytes: &[u8], start: usize, end: usize) -> impl Iterator<Item = usize> + '_ {
    // Use the 2.1.3 stat shape as a discovery hint. The two known level positions
    // cover the name-based and pattern-based layouts.
    let ranges = [
        50.0..=1e6,
        0.5..=2e4,
        0.000001..=10.0,
        0.01..=200.0,
        1.2..=200.0,
        0.5..=3.0,
        10.0..=1e4,
        0.99..=1e6,
    ];
    (start.next_multiple_of(4)..end)
        .step_by(4)
        .filter(move |&offset| {
            let Some(stats) = bytes.get(offset..offset + STAT_BYTES) else {
                return false;
            };
            stats
                .chunks_exact(4)
                .zip(&ranges)
                .all(|(raw, range)| range.contains(&f32::from_le_bytes(raw.try_into().unwrap())))
                && [
                    offset.checked_sub(LEVEL_DISTANCE),
                    offset.checked_add(LEVEL_DISTANCE),
                ]
                .into_iter()
                .flatten()
                .filter_map(|position| bytes.get(position..position + 4))
                .any(|raw| (1..=10_000).contains(&u32::from_le_bytes(raw.try_into().unwrap())))
        })
}

fn name_headers(before: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
    let start = before.len().saturating_sub(NAME_REACH).next_multiple_of(8);
    (start..before.len().saturating_sub(23))
        .step_by(8)
        .filter_map(|offset| {
            let header = &before[offset..offset + 24];
            let capacity = u64::from_le_bytes(header[..8].try_into().unwrap());
            let pointer = u64::from_le_bytes(header[8..16].try_into().unwrap());
            let length = u64::from_le_bytes(header[16..].try_into().unwrap());
            (length >= 2
                && length <= 24
                && length <= capacity
                && capacity <= 256
                && (0x10000..0x7fff_ffff_f000).contains(&pointer))
            .then_some((pointer as usize, length as usize))
        })
}

fn plausible_name(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok_and(|name| {
        let mut chars = name.chars();
        chars.next().is_some_and(char::is_alphabetic)
            && chars.all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '\'' | ' ' | '-'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_stat_block_only_in_its_own_chunk() {
        let mut bytes = [0; 512];
        let position = 256;
        let stats = [210.0_f32, 1.83, 1.0, 0.04, 1.5, 1.0, 40.0, 1.0];
        for (slot, stat) in bytes[position..position + STAT_BYTES]
            .chunks_exact_mut(4)
            .zip(stats)
        {
            slot.copy_from_slice(&stat.to_le_bytes());
        }
        assert_eq!(candidate_offsets(&bytes, 0, bytes.len()).next(), None);
        let level = position + LEVEL_DISTANCE;
        bytes[level..level + 4].copy_from_slice(&3_u32.to_le_bytes());
        assert_eq!(candidate_offsets(&bytes, 0, position).next(), None);
        assert_eq!(
            candidate_offsets(&bytes, position, bytes.len()).collect::<Vec<_>>(),
            [position]
        );
    }

    #[test]
    fn accepts_shifted_text_headers_and_rejects_non_names() {
        let mut bytes = [0_u8; 1600];
        let offset = 496;
        bytes[offset..offset + 8].copy_from_slice(&8_u64.to_le_bytes());
        bytes[offset + 8..offset + 16].copy_from_slice(&0x20000_u64.to_le_bytes());
        bytes[offset + 16..offset + 24].copy_from_slice(&4_u64.to_le_bytes());
        assert_eq!(name_headers(&bytes).collect::<Vec<_>>(), [(0x20000, 4)]);
        assert!(plausible_name(b"Hero"));
        assert!(!plausible_name(b" 42%"));
    }

    #[test]
    fn opens_the_expected_process_and_reads_known_bytes() {
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        let created = game::creation_time(unsafe { GetCurrentProcess() }).unwrap();
        let instance = game::ProcessInstance {
            pid: std::process::id(),
            created: Some(created),
        };
        let memory = Memory::open(instance).unwrap();
        let expected = [12_u8, 34, 56, 78];
        let mut actual = [0; 4];
        memory
            .read(expected.as_ptr() as usize, &mut actual)
            .unwrap();
        assert_eq!(actual, expected);
        assert!(
            Memory::open(game::ProcessInstance {
                created: None,
                ..instance
            })
            .is_err()
        );
    }
}
