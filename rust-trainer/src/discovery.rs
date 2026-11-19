use crate::{game, hud};
use std::{
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
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
const TARGET_FIELD_BYTES: usize = 56;
const LEVEL_DISTANCE: usize = 104;
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);
const VERIFIED_BUILD: &str = "ba09f96a327cadc24371b9dd8d876418c8d6dde298847fab318847be8c44cc5a";

pub struct Target {
    pub name: &'static str,
    panel_label: &'static str,
    offset: usize,
    value: f32,
    baseline: f32,
}

impl Target {
    pub fn displayed_bonus(&self) -> String {
        format!("{:+.0}%", (self.value - self.baseline) * 100.0)
    }

    pub fn panel_value(&self, readings: &[String]) -> Option<f32> {
        let percent = readings
            .iter()
            .find_map(|stat| stat.strip_prefix(self.panel_label)?.strip_prefix(' '))?
            .strip_suffix('%')?
            .parse::<f32>()
            .ok()?;
        let value = self.baseline + percent / 100.0;
        (0.0..=1e6).contains(&value).then_some(value)
    }
}

pub type ActiveStats = [bool; 4];
pub const TARGETS: [Target; 4] = [
    Target {
        name: "Damage",
        panel_label: "Damage",
        offset: 4,
        value: 100.99,
        baseline: 1.0,
    },
    Target {
        name: "Area",
        panel_label: "Area",
        offset: 44,
        value: 100.99,
        baseline: 1.0,
    },
    Target {
        name: "Luck",
        panel_label: "Luck",
        offset: 48,
        value: 99.99,
        baseline: 0.0,
    },
    Target {
        name: "Gold Bonus",
        panel_label: "Gold",
        offset: 52,
        value: 100.99,
        baseline: 1.0,
    },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub build: String,
    pub active: ActiveStats,
}

pub struct Job {
    cancel: Arc<AtomicBool>,
    observation: Arc<Mutex<Option<hud::Player>>>,
    pub result: Receiver<io::Result<Report>>,
}

impl Job {
    pub fn start(
        path: PathBuf,
        instance: game::ProcessInstance,
        character: String,
        observed: Option<hud::Player>,
    ) -> io::Result<Self> {
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let observation = Arc::new(Mutex::new(observed));
        let worker_observation = Arc::clone(&observation);
        let (sender, result) = mpsc::channel();
        thread::Builder::new()
            .name("stat-discovery".into())
            .spawn(move || {
                let work = || {
                    let memory = Memory::open(instance, false)?;
                    let build = game::build_id(&path)?;
                    check_cancel(&worker_cancel)?;
                    if build != VERIFIED_BUILD {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            format!("Freezing is unavailable for game build {}.", &build[..12]),
                        ));
                    }
                    let _ = sender.send(Ok(Report {
                        build: build.clone(),
                        active: ActiveStats::default(),
                    }));
                    let candidates = memory.scan(&character, &worker_cancel)?;
                    let (actor, active) = memory.verify_actor(
                        &candidates,
                        &character,
                        &worker_observation,
                        &worker_cancel,
                    )?;
                    let writer = Memory::open(instance, true)?;
                    writer.freeze(
                        actor,
                        active,
                        &character,
                        &worker_observation,
                        &worker_cancel,
                        |active| {
                            let _ = sender.send(Ok(Report {
                                build: build.clone(),
                                active,
                            }));
                        },
                    )?;
                    Ok::<(), io::Error>(())
                };
                if let Err(error) = work() {
                    let _ = sender.send(Err(error));
                }
            })?;
        Ok(Self {
            cancel,
            observation,
            result,
        })
    }

    pub fn observe(&self, player: hud::Player) {
        *self.observation.lock().unwrap() = Some(player);
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
    health: f32,
    stats: [f32; 4],
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
        let mut bytes = [0; LEVEL_DISTANCE + TARGET_FIELD_BYTES];
        self.read(base - LEVEL_DISTANCE, &mut bytes)?;
        let level = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        let hp = f32::from_le_bytes(
            bytes[LEVEL_DISTANCE..LEVEL_DISTANCE + 4]
                .try_into()
                .unwrap(),
        );
        let stats = TARGETS.map(|target| {
            let index = LEVEL_DISTANCE + target.offset;
            f32::from_le_bytes(bytes[index..index + 4].try_into().unwrap())
        });
        if !(1..=10_000).contains(&level) || !(50.0..=1e6).contains(&hp) {
            return Err(io::Error::other("Character level or health is invalid."));
        }
        Ok(Actor {
            base,
            header,
            level,
            health: hp,
            stats,
        })
    }

    fn verify_actor(
        &self,
        candidates: &[(usize, usize)],
        character: &str,
        observation: &Mutex<Option<hud::Player>>,
        cancel: &AtomicBool,
    ) -> io::Result<(Actor, ActiveStats)> {
        if candidates.is_empty() {
            return Err(io::Error::other(
                "The named solo character is not in memory yet.",
            ));
        }
        let deadline = Instant::now() + Duration::from_secs(50);
        while Instant::now() < deadline {
            check_cancel(cancel)?;
            let current = candidates
                .iter()
                .filter_map(|&(base, header)| self.actor(base, header, character).ok());
            let player = observation.lock().unwrap().clone();
            if let Some(selected) = player
                .as_ref()
                .filter(|player| player.name.eq_ignore_ascii_case(character))
                .and_then(|player| unique_panel_match(current, player))
            {
                return Ok(selected);
            }
            thread::sleep(POLL_INTERVAL);
        }
        Err(io::Error::other(
            "Keep the character panel open while the watcher verifies the live record.",
        ))
    }

    fn freeze(
        &self,
        mut actor: Actor,
        mut active: ActiveStats,
        character: &str,
        observation: &Mutex<Option<hud::Player>>,
        cancel: &AtomicBool,
        mut on_active: impl FnMut(ActiveStats),
    ) -> io::Result<()> {
        let mut reported = ActiveStats::default();
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
            if !active.iter().all(|&enabled| enabled)
                && let Some(player) = observation
                    .lock()
                    .unwrap()
                    .as_ref()
                    .filter(|player| player.name.eq_ignore_ascii_case(character))
            {
                active = extend_active(active, matched_stats(actor, player));
            }
            for (target, enabled) in TARGETS.iter().zip(active) {
                if !enabled {
                    continue;
                }
                check_cancel(cancel)?;
                self.write_stat(actor.base + target.offset, target.value)?;
            }
            if reported != active {
                on_active(active);
                reported = active;
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

    fn scan(&self, character: &str, cancel: &AtomicBool) -> io::Result<Vec<(usize, usize)>> {
        let mut matching = Vec::new();
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
                    Ok(matching)
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
                    continue;
                }
                for offset in candidate_offsets(bytes, cursor - from, last - from) {
                    check_cancel(cancel)?;
                    for (header, pointer, length) in name_headers(&bytes[..offset]) {
                        if length != character.len() {
                            continue;
                        }
                        let mut buffer = [0; 24];
                        let name = &mut buffer[..length];
                        if self.read(pointer, name).is_err() {
                            continue;
                        }
                        if name.eq_ignore_ascii_case(character.as_bytes()) {
                            matching.push((from + offset, from + header));
                            break;
                        }
                    }
                }
            }
        }
    }
}

fn matched_stats(actor: Actor, player: &hud::Player) -> ActiveStats {
    if player.captured.elapsed() > Duration::from_secs(5)
        || (player.level.is_none() && player.health.is_none())
        || player.level.is_some_and(|level| level != actor.level)
        || player
            .health
            .is_some_and(|health| (health - actor.health).abs() > 1.1)
    {
        return ActiveStats::default();
    }
    std::array::from_fn(|index| {
        player.stats[index].is_some_and(|value| (value - actor.stats[index]).abs() <= 0.0051)
    })
}

fn unique_panel_match(
    actors: impl Iterator<Item = Actor>,
    player: &hud::Player,
) -> Option<(Actor, ActiveStats)> {
    let mut matches = actors.filter_map(|actor| {
        let active = matched_stats(actor, player);
        active
            .iter()
            .any(|&enabled| enabled)
            .then_some((actor, active))
    });
    let selection = matches.next()?;
    matches.next().is_none().then_some(selection)
}

fn extend_active(active: ActiveStats, matched: ActiveStats) -> ActiveStats {
    std::array::from_fn(|index| active[index] || matched[index])
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_values_match_the_verified_game_layout() {
        assert_eq!(
            TARGETS.map(|target| (target.offset, target.value)),
            [(4, 100.99), (44, 100.99), (48, 99.99), (52, 100.99)]
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
    fn accepts_shifted_text_headers() {
        let mut bytes = [0_u8; 1600];
        let offset = 496;
        bytes[offset..offset + 8].copy_from_slice(&8_u64.to_le_bytes());
        bytes[offset + 8..offset + 16].copy_from_slice(&0x20000_u64.to_le_bytes());
        bytes[offset + 16..offset + 24].copy_from_slice(&4_u64.to_le_bytes());
        assert_eq!(
            name_headers(&bytes).collect::<Vec<_>>(),
            [(offset, 0x20000, 4)]
        );
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
    fn starts_with_one_matching_stat_and_retains_it_when_more_appear() {
        let mut player = hud::Player {
            name: "Hero".into(),
            level: Some(20),
            health: None,
            stats: [None, Some(1.36), None, None],
            captured: Instant::now(),
        };
        let live = Actor {
            base: 2000,
            header: 1200,
            level: 20,
            health: 839.3,
            stats: [3.338, 1.36, 0.0, 1.31],
        };
        let stale = Actor {
            base: 3000,
            level: 1,
            ..live
        };
        let (actor, active) = unique_panel_match([stale, live].into_iter(), &player).unwrap();
        assert_eq!(actor.base, live.base);
        assert_eq!(active, [false, true, false, false]);

        player.stats = [None, None, None, Some(1.31)];
        let active = extend_active(active, matched_stats(live, &player));
        assert_eq!(active, [false, true, false, true]);
        player.stats = [Some(3.34), None, Some(0.0), None];
        assert_eq!(
            extend_active(active, matched_stats(live, &player)),
            [true; 4]
        );
    }

    #[test]
    fn ambiguous_or_stale_panel_readings_do_not_activate_a_stat() {
        let player = hud::Player {
            name: "Hero".into(),
            level: None,
            health: Some(839.0),
            stats: [None, None, Some(0.0), None],
            captured: Instant::now(),
        };
        let live = Actor {
            base: 2000,
            header: 1200,
            level: 20,
            health: 839.3,
            stats: [3.338, 1.36, 0.0, 1.31],
        };
        let duplicate = Actor { base: 4000, ..live };
        assert!(unique_panel_match([duplicate, live].into_iter(), &player).is_none());
        let old = hud::Player {
            captured: Instant::now() - Duration::from_secs(6),
            ..player
        };
        assert!(unique_panel_match([live].into_iter(), &old).is_none());
    }
}
