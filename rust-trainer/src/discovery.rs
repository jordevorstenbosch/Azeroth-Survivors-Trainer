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
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, HANDLE},
    System::{
        Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory},
        Memory::{
            MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_READWRITE, PAGE_WRITECOPY,
            VirtualQueryEx,
        },
        Threading::{
            OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_READ,
            PROCESS_VM_WRITE,
        },
    },
};

const CHUNK: usize = 512 * 1024;
const NAME_REACH: usize = 1536;
const STAT_BYTES: usize = 32;
const LEVEL_DISTANCE: usize = 104;
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);
const VERIFIED_BUILD: &str = "ba09f96a327cadc24371b9dd8d876418c8d6dde298847fab318847be8c44cc5a";

pub struct Target {
    pub name: &'static str,
    offset: usize,
    value: f32,
    baseline: f32,
}

impl Target {
    pub fn displayed_bonus(&self) -> String {
        format!("{:+.0}%", (self.value - self.baseline) * 100.0)
    }
}

pub const TARGETS: [Target; 4] = [
    Target {
        name: "Damage",
        offset: 4,
        value: 100.99,
        baseline: 1.0,
    },
    Target {
        name: "Area",
        offset: 44,
        value: 100.99,
        baseline: 1.0,
    },
    Target {
        name: "Luck",
        offset: 48,
        value: 99.99,
        baseline: 0.0,
    },
    Target {
        name: "Gold Bonus",
        offset: 52,
        value: 100.99,
        baseline: 1.0,
    },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub build: String,
    pub candidates: usize,
    pub unreadable_chunks: usize,
    pub freezing: bool,
    pub awaiting_progress: bool,
    matching: Vec<(usize, usize)>,
}

pub struct Job {
    cancel: Arc<AtomicBool>,
    pub result: Receiver<io::Result<Report>>,
}

impl Job {
    pub fn start(
        path: PathBuf,
        instance: game::ProcessInstance,
        solo_character: Option<String>,
    ) -> io::Result<Self> {
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, result) = mpsc::channel();
        thread::Builder::new()
            .name("stat-discovery".into())
            .spawn(move || {
                let work = || {
                    let memory = Memory::open(instance, false)?;
                    let build = game::build_id(&path)?;
                    check_cancel(&worker_cancel)?;
                    let character = solo_character
                        .as_deref()
                        .filter(|_| build == VERIFIED_BUILD);
                    let mut report = memory.scan(build, character, &worker_cancel)?;
                    if let Some(character) = character {
                        report.awaiting_progress = true;
                        let candidates = std::mem::take(&mut report.matching);
                        let _ = sender.send(Ok(report.clone()));
                        let actor =
                            memory.wait_for_progress(&candidates, character, &worker_cancel)?;
                        let writer = Memory::open(instance, true)?;
                        writer.freeze(actor, character, &worker_cancel, || {
                            report.awaiting_progress = false;
                            report.freezing = true;
                            let _ = sender.send(Ok(report.clone()));
                        })?;
                    } else {
                        let _ = sender.send(Ok(report));
                    }
                    Ok::<(), io::Error>(())
                };
                if let Err(error) = work() {
                    let _ = sender.send(Err(error));
                }
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

#[derive(Clone, Copy)]
struct Actor {
    base: usize,
    header: usize,
    level: u32,
}

impl Memory {
    fn open(instance: game::ProcessInstance, writable: bool) -> io::Result<Self> {
        let mut rights = PROCESS_QUERY_INFORMATION | PROCESS_VM_READ;
        if writable {
            rights |= PROCESS_VM_OPERATION | PROCESS_VM_WRITE;
        }
        let handle = unsafe { OpenProcess(rights, 0, instance.pid) };
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

    fn actor(&self, base: usize, header: usize, character: &str) -> io::Result<Actor> {
        let mut name_header = [0; 24];
        self.read(header, &mut name_header)?;
        let (pointer, length) = parse_name_header(&name_header)
            .ok_or_else(|| io::Error::other("Character name header changed."))?;
        let mut name = [0; 24];
        self.read(pointer, &mut name[..length])?;
        if !name[..length].eq_ignore_ascii_case(character.as_bytes()) {
            return Err(io::Error::other("Character name changed."));
        }
        let mut bytes = [0; LEVEL_DISTANCE + STAT_BYTES];
        self.read(base - LEVEL_DISTANCE, &mut bytes)?;
        let level = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        let hp = f32::from_le_bytes(
            bytes[LEVEL_DISTANCE..LEVEL_DISTANCE + 4]
                .try_into()
                .unwrap(),
        );
        if !(1..=10_000).contains(&level) || !(50.0..=1e6).contains(&hp) {
            return Err(io::Error::other("Character level or health is invalid."));
        }
        Ok(Actor {
            base,
            header,
            level,
        })
    }

    fn wait_for_progress(
        &self,
        candidates: &[(usize, usize)],
        character: &str,
        cancel: &AtomicBool,
    ) -> io::Result<Actor> {
        let initial: Vec<_> = candidates
            .iter()
            .filter_map(|&(base, header)| self.actor(base, header, character).ok())
            .collect();
        if initial.is_empty() {
            return Err(io::Error::other(
                "The named solo character is not in memory yet.",
            ));
        }
        let deadline = Instant::now() + Duration::from_secs(50);
        let mut last = None;
        while Instant::now() < deadline {
            check_cancel(cancel)?;
            let current = initial
                .iter()
                .filter_map(|actor| self.actor(actor.base, actor.header, character).ok());
            let advancing = unique_progress(&initial, current);
            if let Some(actor) = advancing {
                if last == Some(actor.base) {
                    return Ok(actor);
                }
            }
            last = advancing.map(|actor| actor.base);
            thread::sleep(POLL_INTERVAL);
        }
        Err(io::Error::other(
            "Waiting for a unique solo character to level up.",
        ))
    }

    fn freeze(
        &self,
        mut actor: Actor,
        character: &str,
        cancel: &AtomicBool,
        started: impl FnOnce(),
    ) -> io::Result<()> {
        let mut started = Some(started);
        let mut next_tick = Instant::now();
        loop {
            check_cancel(cancel)?;
            let fresh = self.actor(actor.base, actor.header, character)?;
            if fresh.level < actor.level {
                return Err(io::Error::other(
                    "The solo run changed. Reacquiring the character.",
                ));
            }
            actor = fresh;
            for target in &TARGETS {
                check_cancel(cancel)?;
                self.write_stat(actor.base + target.offset, target.value)?;
            }
            if let Some(started) = started.take() {
                started();
            }
            next_tick += POLL_INTERVAL;
            let now = Instant::now();
            if next_tick <= now {
                next_tick = now + POLL_INTERVAL;
            }
            thread::sleep(next_tick.duration_since(now));
        }
    }

    fn write_stat(&self, address: usize, value: f32) -> io::Result<()> {
        let expected = value.to_le_bytes();
        let mut written = 0;
        let ok = unsafe {
            WriteProcessMemory(
                self.0,
                address as _,
                expected.as_ptr().cast(),
                expected.len(),
                &mut written,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if written != expected.len() {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        Ok(())
    }

    fn scan(
        &self,
        build: String,
        character: Option<&str>,
        cancel: &AtomicBool,
    ) -> io::Result<Report> {
        let mut report = Report {
            build,
            candidates: 0,
            unreadable_chunks: 0,
            freezing: false,
            awaiting_progress: false,
            matching: Vec::new(),
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
                    let mut plausible = false;
                    for (header, pointer, length) in name_headers(&bytes[..offset]) {
                        let mut buffer = [0; 24];
                        let name = &mut buffer[..length];
                        if self.read(pointer, name).is_err() || !plausible_name(name) {
                            continue;
                        }
                        plausible = true;
                        if character
                            .is_some_and(|expected| name.eq_ignore_ascii_case(expected.as_bytes()))
                        {
                            report.matching.push((from + offset, from + header));
                            break;
                        }
                        if character.is_none() {
                            break;
                        }
                    }
                    report.candidates += usize::from(plausible);
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

fn unique_progress(initial: &[Actor], current: impl Iterator<Item = Actor>) -> Option<Actor> {
    let mut advancing = current.filter(|actor| {
        initial
            .iter()
            .any(|old| old.base == actor.base && old.level < actor.level)
    });
    let actor = advancing.next()?;
    advancing.next().is_none().then_some(actor)
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

fn name_headers(before: &[u8]) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
    let start = before.len().saturating_sub(NAME_REACH).next_multiple_of(8);
    (start..before.len().saturating_sub(23))
        .step_by(8)
        .filter_map(|offset| {
            let (pointer, length) = parse_name_header(&before[offset..offset + 24])?;
            Some((offset, pointer, length))
        })
}

fn parse_name_header(header: &[u8]) -> Option<(usize, usize)> {
    let capacity = u64::from_le_bytes(header[..8].try_into().unwrap());
    let pointer = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let length = u64::from_le_bytes(header[16..].try_into().unwrap());
    ((2..=24).contains(&length)
        && length <= capacity
        && capacity <= 256
        && (0x10000..0x7fff_ffff_f000).contains(&pointer))
    .then_some((pointer as usize, length as usize))
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
    fn displayed_targets_match_the_game_percentage_conventions() {
        assert_eq!(
            TARGETS.map(|target| target.displayed_bonus()),
            ["+9999%"; 4]
        );
    }

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
        assert_eq!(
            name_headers(&bytes).collect::<Vec<_>>(),
            [(offset, 0x20000, 4)]
        );
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
        let memory = Memory::open(instance, true).unwrap();
        let expected = [12_u8, 34, 56, 78];
        let mut actual = [0; 4];
        memory
            .read(expected.as_ptr() as usize, &mut actual)
            .unwrap();
        assert_eq!(actual, expected);
        let mut stat = 1.0_f32;
        let address = std::ptr::addr_of_mut!(stat) as usize;
        memory.write_stat(address, TARGETS[0].value).unwrap();
        let mut readback = [0; 4];
        memory.read(address, &mut readback).unwrap();
        assert_eq!(f32::from_le_bytes(readback), 100.99);
        assert!(
            Memory::open(
                game::ProcessInstance {
                    created: None,
                    ..instance
                },
                false
            )
            .is_err()
        );
    }

    #[test]
    fn level_progress_distinguishes_the_live_record_from_a_stale_copy() {
        let stale = Actor {
            base: 1000,
            header: 200,
            level: 1,
        };
        let live = Actor {
            base: 2000,
            header: 1200,
            level: 20,
        };
        let initial = [stale, live];
        assert!(unique_progress(&initial, initial.into_iter()).is_none());

        let next = Actor { level: 21, ..live };
        assert_eq!(
            unique_progress(&initial, [stale, next].into_iter()).map(|actor| actor.base),
            Some(live.base)
        );

        let duplicate = Actor { level: 2, ..stale };
        assert!(unique_progress(&initial, [duplicate, next].into_iter()).is_none());
    }
}
