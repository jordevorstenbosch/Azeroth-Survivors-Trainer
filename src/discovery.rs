use crate::{diagnostic_log, game, hud};
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
const STAT_RANGES: [std::ops::RangeInclusive<f32>; 8] = [
    50.0..=1e6,
    0.5..=2e4,
    0.000001..=10.0,
    0.01..=200.0,
    1.2..=200.0,
    0.5..=3.0,
    10.0..=1e4,
    0.99..=1e6,
];
const STAT_NAMES: [&str; 8] = [
    "HP", "Damage", "Haste", "Crit", "Crit damage", "Speed", "Pickup", "XP",
];
const LEVEL_DISTANCE: usize = 104;
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);
const VERIFIED_BUILD: &str = "ba09f96a327cadc24371b9dd8d876418c8d6dde298847fab318847be8c44cc5a";
const MAX_PACKET: usize = 1024;

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
type WriteOffsets = [Option<usize>; 4];
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
const CURRENT_OFFSETS: [usize; 4] = [4, 44, 48, 52];
const HISTORICAL_OFFSETS: [usize; 4] = [4, 40, 44, 48];

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
                    let known_build = build == VERIFIED_BUILD;
                    let _ = sender.send(Ok(Report {
                        build: build.clone(),
                        active: ActiveStats::default(),
                    }));
                    diagnostic_log::info(&format!(
                        "Discovery started for {character}. Build {} known.",
                        if known_build { "is" } else { "is not" }
                    ));
                    let scan_started = Instant::now();
                    let panel = worker_observation.lock().ok().and_then(|observed| {
                        let player = observed.as_ref()?;
                        (player.name.eq_ignore_ascii_case(&character)
                            && player.captured.elapsed() <= Duration::from_secs(5))
                            .then_some((player.health?, player.level?))
                    });
                    let candidates = memory.scan(&character, &worker_cancel, known_build, panel)?;
                    diagnostic_log::info(&format!(
                        "Memory scan found {} candidate records in {} ms.",
                        candidates.len(),
                        scan_started.elapsed().as_millis()
                    ));
                    let verify_started = Instant::now();
                    let (actor, offsets) = memory.verify_actor(
                        &candidates,
                        &character,
                        &worker_observation,
                        &worker_cancel,
                        known_build,
                    )?;
                    diagnostic_log::info(&format!(
                        "Live record verified in {} ms. Stat offsets {:?}.",
                        verify_started.elapsed().as_millis(),
                        offsets
                    ));
                    let writer = Memory::open(instance, true)?;
                    writer.freeze(
                        actor,
                        offsets,
                        &character,
                        &worker_observation,
                        &worker_cancel,
                        known_build,
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

// SAFETY: ReadProcessMemory and WriteProcessMemory may be called on one process
// handle from several threads, and the scan only reads through the handle.
unsafe impl Send for Memory {}
unsafe impl Sync for Memory {}

struct Chunk {
    from: usize,
    to: usize,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy)]
struct Actor {
    base: usize,
    header: usize,
    level: u32,
    experience: u32,
    health: f32,
    stats: [f32; 4],
    historical_area: f32,
    level_after_stats: bool,
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

    fn actor(&self, record: Actor, character: &str) -> io::Result<Actor> {
        let actual = self.actor_name(record.header)?;
        if !actual.eq_ignore_ascii_case(character) {
            return Err(io::Error::other("Character name changed."));
        }
        self.actor_stats(record.base, record.header, record.level_after_stats)
    }

    fn actor_name(&self, header: usize) -> io::Result<String> {
        let mut name_header = [0; 24];
        self.read(header, &mut name_header)?;
        let (pointer, length) = parse_name_header(&name_header)
            .ok_or_else(|| io::Error::other("Character name header changed."))?;
        let mut name = [0; 24];
        self.read(pointer, &mut name[..length])?;
        let name = std::str::from_utf8(&name[..length]).map_err(io::Error::other)?;
        if name.trim().is_empty() || name.chars().any(char::is_control) {
            return Err(io::Error::other("Invalid character name."));
        }
        Ok(name.to_owned())
    }

    fn actor_stats(
        &self,
        base: usize,
        header: usize,
        level_after_stats: bool,
    ) -> io::Result<Actor> {
        let mut bytes = [0; 2 * LEVEL_DISTANCE + 8];
        let size = if level_after_stats {
            bytes.len()
        } else {
            LEVEL_DISTANCE + 56
        };
        self.read(base - LEVEL_DISTANCE, &mut bytes[..size])?;
        let level_index = if level_after_stats {
            2 * LEVEL_DISTANCE
        } else {
            0
        };
        let level = read_u32(&bytes, level_index).unwrap();
        let experience = read_u32(&bytes, level_index + 4).unwrap();
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
            experience,
            health: hp,
            stats,
            historical_area: f32::from_bits(read_u32(&bytes, LEVEL_DISTANCE + 40).unwrap()),
            level_after_stats,
        })
    }

    fn verify_actor(
        &self,
        candidates: &[(usize, usize, bool)],
        character: &str,
        observation: &Mutex<Option<hud::Player>>,
        cancel: &AtomicBool,
        known_build: bool,
    ) -> io::Result<(Actor, WriteOffsets)> {
        if candidates.is_empty() {
            return Err(io::Error::other(
                "The named solo character is not in memory yet.",
            ));
        }
        let deadline = Instant::now() + Duration::from_secs(50);
        let mut previous = HashMap::new();
        let mut recent_progress = HashMap::new();
        while Instant::now() < deadline {
            check_cancel(cancel)?;
            let current: Vec<_> = candidates
                .iter()
                .filter_map(|&(base, header, level_after_stats)| {
                    self.actor_stats(base, header, level_after_stats)
                        .ok()
                        .filter(|_| {
                            self.actor_name(header)
                                .is_ok_and(|name| name.eq_ignore_ascii_case(character))
                        })
                })
                .collect();
            let now = Instant::now();
            recent_progress
                .retain(|_, observed| now.duration_since(*observed) <= Duration::from_secs(5));
            for &actor in &current {
                let key = actor_key(actor);
                if progressed(&previous, actor) {
                    recent_progress.insert(key, now);
                } else if previous.get(&key).is_none_or(|old| actor.level < old.level) {
                    recent_progress.remove(&key);
                }
            }
            let player = observation.lock().unwrap().clone();
            if let Some(selected) = player
                .as_ref()
                .filter(|player| player.name.eq_ignore_ascii_case(character))
                .and_then(|player| {
                    unique_panel_match(
                        current.iter().copied().filter(|actor| {
                            known_build || recent_progress.contains_key(&actor_key(*actor))
                        }),
                        player,
                        known_build,
                    )
                })
            {
                return Ok(selected);
            }
            previous = actor_samples(current);
            thread::sleep(POLL_INTERVAL);
        }
        Err(io::Error::other(
            "Keep the character panel open while the watcher verifies the live record.",
        ))
    }

    fn freeze(
        &self,
        mut actor: Actor,
        mut offsets: WriteOffsets,
        character: &str,
        observation: &Mutex<Option<hud::Player>>,
        cancel: &AtomicBool,
        known_build: bool,
        mut on_active: impl FnMut(ActiveStats),
    ) -> io::Result<()> {
        let mut reported = ActiveStats::default();
        let mut next_tick = Instant::now();
        loop {
            check_cancel(cancel)?;
            let fresh = self.actor(actor, character)?;
            if fresh.level < actor.level {
                return Err(io::Error::other(
                    "The solo run changed. Reacquiring the character.",
                ));
            }
            actor = fresh;
            if offsets.contains(&None)
                && let Some(player) = observation
                    .lock()
                    .unwrap()
                    .as_ref()
                    .filter(|player| player.name.eq_ignore_ascii_case(character))
            {
                offsets = panel_offsets(actor, player, known_build, offsets);
            }
            for (target, offset) in TARGETS
                .iter()
                .zip(offsets)
                .filter_map(|(target, offset)| Some((target, offset?)))
            {
                check_cancel(cancel)?;
                self.write_stat(actor.base + offset, target.value)?;
            }
            let active = offsets.map(|offset| offset.is_some());
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

    fn scan(
        &self,
        character: &str,
        cancel: &AtomicBool,
        known_build: bool,
        panel: Option<(f32, u32)>,
    ) -> io::Result<Vec<(usize, usize, bool)>> {
        let stat_matches = AtomicUsize::new(0);
        let level_matches = AtomicUsize::new(0);
        let header_matches = AtomicUsize::new(0);
        let readable_names = AtomicUsize::new(0);
        let panel_health_matches = AtomicUsize::new(0);
        let panel_stat_matches = AtomicUsize::new(0);
        let panel_level_before = AtomicUsize::new(0);
        let panel_level_after = AtomicUsize::new(0);
        let panel_stat_and_level_matches = AtomicUsize::new(0);
        let failures: [AtomicUsize; 8] = std::array::from_fn(|_| AtomicUsize::new(0));
        let incomplete_blocks = AtomicUsize::new(0);
        let named_panel_blocks = AtomicUsize::new(0);
        let candidates = self.scan_chunks(cancel, |from, bytes, start, end, matching| {
            let mut chunk_stat_matches = 0;
            let mut chunk_level_matches = 0;
            let mut chunk_header_matches = 0;
            let mut chunk_readable_names = 0;
            if let Some((health, level)) = panel {
                for offset in (start.next_multiple_of(4)..end).step_by(4) {
                    if !matches_panel_health(bytes, offset, health) {
                        continue;
                    }
                    panel_health_matches.fetch_add(1, Ordering::Relaxed);
                    let level_before = offset.checked_sub(LEVEL_DISTANCE)
                        .and_then(|at| read_u32(bytes, at)) == Some(level);
                    let level_after = offset.checked_add(LEVEL_DISTANCE)
                        .and_then(|at| read_u32(bytes, at)) == Some(level);
                    if level_before {
                        panel_level_before.fetch_add(1, Ordering::Relaxed);
                    }
                    if level_after {
                        panel_level_after.fetch_add(1, Ordering::Relaxed);
                    }
                    if !level_before && !level_after {
                        continue;
                    }
                    let Some(stats) = bytes.get(offset..offset + STAT_BYTES) else {
                        incomplete_blocks.fetch_add(1, Ordering::Relaxed);
                        continue;
                    };
                    let mut rejected = Vec::new();
                    for (index, (raw, range)) in stats.chunks_exact(4).zip(&STAT_RANGES).enumerate() {
                        let value = f32::from_le_bytes(raw.try_into().unwrap());
                        if !range.contains(&value) {
                            failures[index].fetch_add(1, Ordering::Relaxed);
                            rejected.push(format!("{}={value:?}", STAT_NAMES[index]));
                        }
                    }
                    if name_headers(&bytes[..offset]).any(|(header, _, length)| {
                        length == character.len() && self.actor_name(from + header)
                            .is_ok_and(|name| name.eq_ignore_ascii_case(character))
                    }) {
                        named_panel_blocks.fetch_add(1, Ordering::Relaxed);
                        if !rejected.is_empty() {
                            diagnostic_log::info(&format!(
                                "Stat bounds rejected a block matching panel name, HP, and level: {}.",
                                rejected.join(", ")
                            ));
                        }
                    }
                }
            }
            for offset in candidate_offsets(bytes, start, end, |offset| {
                chunk_stat_matches += 1;
                if panel.is_some_and(|(health, _)| matches_panel_health(bytes, offset, health)) {
                    panel_stat_matches.fetch_add(1, Ordering::Relaxed);
                }
            }) {
                check_cancel(cancel)?;
                chunk_level_matches += 1;
                if panel.is_some_and(|(health, level)| {
                    matches_panel_health(bytes, offset, health)
                        && [offset.checked_sub(LEVEL_DISTANCE), offset.checked_add(LEVEL_DISTANCE)]
                            .into_iter()
                            .flatten()
                            .any(|at| read_u32(bytes, at) == Some(level))
                }) {
                    panel_stat_and_level_matches.fetch_add(1, Ordering::Relaxed);
                }
                for (header, _, length) in name_headers(&bytes[..offset]) {
                    if length != character.len() {
                        continue;
                    }
                    chunk_header_matches += 1;
                    let Ok(name) = self.actor_name(from + header) else {
                        continue;
                    };
                    chunk_readable_names += 1;
                    if name.eq_ignore_ascii_case(character) {
                        matching.push((from + offset, from + header, false));
                        if !known_build {
                            matching.push((from + offset, from + header, true));
                        }
                        break;
                    }
                }
            }
            stat_matches.fetch_add(chunk_stat_matches, Ordering::Relaxed);
            level_matches.fetch_add(chunk_level_matches, Ordering::Relaxed);
            header_matches.fetch_add(chunk_header_matches, Ordering::Relaxed);
            readable_names.fetch_add(chunk_readable_names, Ordering::Relaxed);
            Ok(())
        })?;
        diagnostic_log::info(&format!(
            "Discovery filters: {} stat blocks, {} also match level, {} same-length name headers, {} readable names, {} candidate records.",
            stat_matches.load(Ordering::Relaxed),
            level_matches.load(Ordering::Relaxed),
            header_matches.load(Ordering::Relaxed),
            readable_names.load(Ordering::Relaxed),
            candidates.len(),
        ));
        if let Some((health, level)) = panel {
            diagnostic_log::info(&format!(
                "Panel correlation (HP {health}, level {level}): {} HP matches, {} pass the stat filter, {} have the exact level at -{LEVEL_DISTANCE}, {} at +{LEVEL_DISTANCE}, {} pass the stat filter and have the exact level at either position.",
                panel_health_matches.load(Ordering::Relaxed),
                panel_stat_matches.load(Ordering::Relaxed),
                panel_level_before.load(Ordering::Relaxed),
                panel_level_after.load(Ordering::Relaxed),
                panel_stat_and_level_matches.load(Ordering::Relaxed),
            ));
            let rejected = STAT_NAMES.iter().zip(&failures)
                .map(|(name, count)| format!("{name} {}", count.load(Ordering::Relaxed)))
                .collect::<Vec<_>>().join(", ");
            diagnostic_log::info(&format!(
                "Stat-bound rejections among exact-level HP matches: {rejected}. Incomplete blocks: {}. Matching name headers: {}.",
                incomplete_blocks.load(Ordering::Relaxed),
                named_panel_blocks.load(Ordering::Relaxed),
            ));
        } else {
            diagnostic_log::info("Panel correlation skipped: need a fresh reading with both HP and level.");
        }
        Ok(candidates)
    }

    // Lists the readable memory as chunks, then scans them on several threads.
    fn scan_chunks<T: Send>(
        &self,
        cancel: &AtomicBool,
        visit: impl Fn(usize, &[u8], usize, usize, &mut Vec<T>) -> io::Result<()> + Sync,
    ) -> io::Result<Vec<T>> {
        let chunks = self.readable_chunks(cancel)?;
        let next = AtomicUsize::new(0);
        let workers = thread::available_parallelism()
            .map_or(4, |count| count.get())
            .clamp(1, 8);
        thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    scope.spawn(|| -> io::Result<Vec<T>> {
                        let mut found = Vec::new();
                        let mut buffer = vec![0; CHUNK + NAME_REACH + MAX_PACKET];
                        loop {
                            check_cancel(cancel)?;
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            let Some(chunk) = chunks.get(index) else {
                                return Ok(found);
                            };
                            let bytes = &mut buffer[..chunk.to - chunk.from];
                            if self.read(chunk.from, bytes).is_err() {
                                continue;
                            }
                            visit(chunk.from, bytes, chunk.start, chunk.end, &mut found)?;
                        }
                    })
                })
                .collect();
            let mut results = Vec::new();
            let mut failure = None;
            for handle in handles {
                match handle.join() {
                    Ok(Ok(mut found)) => results.append(&mut found),
                    Ok(Err(error)) => {
                        failure.get_or_insert(error);
                    }
                    Err(_) => {
                        failure.get_or_insert(io::Error::other("The memory scan stopped."));
                    }
                }
            }
            match failure {
                Some(error) => Err(error),
                None => Ok(results),
            }
        })
    }

    fn readable_chunks(&self, cancel: &AtomicBool) -> io::Result<Vec<Chunk>> {
        let mut chunks = Vec::new();
        let mut next = 0;
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
                    Ok(chunks)
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
                let from = cursor.saturating_sub(NAME_REACH).max(start);
                let last = cursor.saturating_add(CHUNK).min(end);
                let to = last.saturating_add(MAX_PACKET).min(end);
                chunks.push(Chunk {
                    from,
                    to,
                    start: cursor - from,
                    end: last - from,
                });
            }
        }
    }
}

type ActorSamples = HashMap<(usize, usize, bool), Actor>;

fn actor_key(actor: Actor) -> (usize, usize, bool) {
    (actor.base, actor.header, actor.level_after_stats)
}

fn actor_samples(actors: Vec<Actor>) -> ActorSamples {
    actors
        .into_iter()
        .map(|actor| (actor_key(actor), actor))
        .collect()
}

fn progressed(previous: &ActorSamples, actor: Actor) -> bool {
    previous.get(&actor_key(actor)).is_some_and(|old| {
        actor.level > old.level
            || (actor.level == old.level
                && !actor.level_after_stats
                && actor.experience > old.experience)
    })
}

fn panel_offsets(
    actor: Actor,
    player: &hud::Player,
    known_build: bool,
    bound: WriteOffsets,
) -> WriteOffsets {
    if player.captured.elapsed() > Duration::from_secs(5)
        || (player.level.is_none() && player.health.is_none())
        || player.level.is_some_and(|level| level != actor.level)
        || player
            .health
            .is_some_and(|health| (health - actor.health).abs() > 1.1)
    {
        return bound;
    }
    let observed = |index: usize, value: f32| {
        player.stats[index].is_some_and(|expected| (expected - value).abs() <= 0.0051)
    };
    if known_build {
        return std::array::from_fn(|index| {
            bound[index]
                .or_else(|| observed(index, actor.stats[index]).then_some(CURRENT_OFFSETS[index]))
        });
    }
    let historical_values = [
        actor.stats[0],
        actor.historical_area,
        actor.stats[1],
        actor.stats[2],
    ];
    let compatible = |offsets: [usize; 4], values: [f32; 4]| {
        (0..4).all(|index| match bound[index] {
            Some(offset) => offset == offsets[index],
            None => player.stats[index].is_none() || observed(index, values[index]),
        })
    };
    let current = compatible(CURRENT_OFFSETS, actor.stats);
    let historical = compatible(HISTORICAL_OFFSETS, historical_values);
    std::array::from_fn(|index| {
        bound[index].or_else(|| {
            player.stats[index]?;
            match (current, historical) {
                (true, false) => Some(CURRENT_OFFSETS[index]),
                (false, true) => Some(HISTORICAL_OFFSETS[index]),
                (true, true) if CURRENT_OFFSETS[index] == HISTORICAL_OFFSETS[index] => {
                    Some(CURRENT_OFFSETS[index])
                }
                _ => None,
            }
        })
    })
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

fn unique_panel_match(
    actors: impl Iterator<Item = Actor>,
    player: &hud::Player,
    known_build: bool,
) -> Option<(Actor, WriteOffsets)> {
    let mut matches = actors.filter_map(|actor| {
        let offsets = panel_offsets(actor, player, known_build, [None; 4]);
        offsets
            .iter()
            .any(Option::is_some)
            .then_some((actor, offsets))
    });
    let selected = matches.next()?;
    matches.next().is_none().then_some(selected)
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn matches_panel_health(bytes: &[u8], offset: usize, health: f32) -> bool {
    read_u32(bytes, offset)
        .is_some_and(|bits| (f32::from_bits(bits) - health).abs() <= 1.1)
}

fn candidate_offsets<'a>(
    bytes: &'a [u8],
    start: usize,
    end: usize,
    mut on_stat_match: impl FnMut(usize) + 'a,
) -> impl Iterator<Item = usize> + 'a {
    // Use the 2.1.3 stat shape as a discovery hint. The two known level positions
    // cover the name-based and pattern-based layouts.
    (start.next_multiple_of(4)..end)
        .step_by(4)
        .filter(move |&offset| {
            let Some(stats) = bytes.get(offset..offset + STAT_BYTES) else {
                return false;
            };
            if !stats
                .chunks_exact(4)
                .zip(&STAT_RANGES)
                .all(|(raw, range)| range.contains(&f32::from_le_bytes(raw.try_into().unwrap())))
            {
                return false;
            }
            on_stat_match(offset);
            [
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
        let mut stat_matches = 0;
        assert_eq!(
            candidate_offsets(&bytes, position, position + 4, |_| stat_matches += 1).next(),
            None
        );
        assert_eq!(stat_matches, 1);
        let level = position + LEVEL_DISTANCE;
        bytes[level..level + 4].copy_from_slice(&3_u32.to_le_bytes());
        assert_eq!(candidate_offsets(&bytes, 0, position, |_| {}).next(), None);
        assert_eq!(
            candidate_offsets(&bytes, position, bytes.len(), |_| {}).collect::<Vec<_>>(),
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
            experience: 10,
            health: 839.3,
            stats: [3.338, 1.36, 0.0, 1.31],
            historical_area: 5.0,
            level_after_stats: false,
        };
        let stale = Actor {
            base: 3000,
            level: 1,
            ..live
        };
        let (actor, active) = unique_panel_match([stale, live].into_iter(), &player, true).unwrap();
        assert_eq!(actor.base, live.base);
        assert_eq!(active, [None, Some(44), None, None]);

        player.stats = [None, None, None, Some(1.31)];
        let active = panel_offsets(live, &player, true, active);
        assert_eq!(active, [None, Some(44), None, Some(52)]);
        player.stats = [Some(3.34), None, Some(0.0), None];
        assert_eq!(
            panel_offsets(live, &player, true, active),
            CURRENT_OFFSETS.map(Some)
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
            experience: 10,
            health: 839.3,
            stats: [3.338, 1.36, 0.0, 1.31],
            historical_area: 5.0,
            level_after_stats: false,
        };
        let duplicate = Actor { base: 4000, ..live };
        assert!(unique_panel_match([duplicate, live].into_iter(), &player, true).is_none());
        let old = hud::Player {
            captured: Instant::now() - Duration::from_secs(6),
            ..player
        };
        assert!(unique_panel_match([live].into_iter(), &old, true).is_none());
    }

    #[test]
    fn unknown_build_discovers_damage_and_then_earlier_layout_offsets() {
        let mut player = hud::Player {
            name: "Hero".into(),
            level: Some(20),
            health: Some(839.0),
            stats: [Some(3.34), None, None, None],
            captured: Instant::now(),
        };
        let live = Actor {
            base: 2000,
            header: 1200,
            level: 20,
            experience: 0,
            health: 839.3,
            stats: [3.338, 0.36, 1.31, 0.0],
            historical_area: 1.36,
            level_after_stats: true,
        };
        let bound = panel_offsets(live, &player, false, [None; 4]);
        assert_eq!(bound, [Some(4), None, None, None]);
        player.stats = [None, Some(1.36), Some(0.36), Some(1.31)];
        assert_eq!(
            panel_offsets(live, &player, false, bound),
            HISTORICAL_OFFSETS.map(Some)
        );

        player.stats = [None, Some(1.36), None, None];
        let ambiguous = Actor {
            stats: [3.338, 1.36, 1.31, 0.0],
            ..live
        };
        assert_eq!(panel_offsets(ambiguous, &player, false, bound), bound);
    }

}
