use crate::{game, hud};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read, Seek},
    path::{Path, PathBuf},
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
const LEVEL_DISTANCE: usize = 104;
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);
const VERIFIED_BUILD: &str = "ba09f96a327cadc24371b9dd8d876418c8d6dde298847fab318847be8c44cc5a";
const WRITER_RVA: usize = 0x5f7f01a;
const WRITER_FILE_OFFSET: u64 = 0x5f7e41a;
const WRITER_HASH: &str = "34ae43d72305c49c602a8194c0757a20753cf546c29d82b61d97001f50ffb50d";
const NAME_DISTANCE: usize = 0x660 - 0x2a8;
const NAME_OBJECT_OFFSET: usize = 0x2a8;
const STAT_PACKET: &[u8] = b"AZS\x09hp=";
const MAX_PACKET: usize = 1024;
const HISTORICAL_NAME_DISTANCES: [usize; 3] = [NAME_DISTANCE, 912, 792];

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
        character: Option<String>,
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
                    let (actor, character, offsets) = if let Some(character) = character {
                        let candidates = memory.scan(&character, &worker_cancel, known_build)?;
                        let (actor, active) = memory.verify_actor(
                            &candidates,
                            &character,
                            &worker_observation,
                            &worker_cancel,
                            known_build,
                        )?;
                        (actor, character, active)
                    } else {
                        let mut distances = vec![NAME_DISTANCE];
                        if known_build {
                            memory.verify_stat_writer(instance, &path)?;
                        } else {
                            let stat_base =
                                memory.find_stat_writer(instance, &path, &worker_cancel)?;
                            distances.extend_from_slice(&HISTORICAL_NAME_DISTANCES[1..]);
                            if let Some(distance) = stat_base.checked_sub(NAME_OBJECT_OFFSET)
                                && distance <= NAME_REACH
                                && !distances.contains(&distance)
                            {
                                distances.push(distance);
                            }
                        }
                        let (actor, character) =
                            memory.verify_native_actor(&worker_cancel, known_build, &distances)?;
                        (actor, character, CURRENT_OFFSETS.map(Some))
                    };
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
    fn find_stat_writer(
        &self,
        instance: game::ProcessInstance,
        path: &Path,
        cancel: &AtomicBool,
    ) -> io::Result<usize> {
        let image = std::fs::read(path)?;
        let (code, rva) = text_section(&image)
            .ok_or_else(|| io::Error::other("The executable has no readable .text section."))?;
        let (start, end, stat_base) = find_writer(code, cancel)?;
        let address = game::module_base(instance.pid, path)?
            .checked_add(rva)
            .and_then(|base| base.checked_add(start))
            .ok_or_else(|| io::Error::other("Invalid stat writer address."))?;
        let mut loaded = vec![0; end - start];
        self.read(address, &mut loaded)?;
        if loaded != code[start..end] {
            return Err(io::Error::other(
                "The loaded stat writer differs from the game file.",
            ));
        }
        Ok(stat_base as usize)
    }

    fn verify_stat_writer(&self, instance: game::ProcessInstance, path: &Path) -> io::Result<()> {
        let mut file = File::open(path)?;
        file.seek(io::SeekFrom::Start(WRITER_FILE_OFFSET))?;
        let mut expected = [0; 350];
        file.read_exact(&mut expected)?;
        let mut loaded = [0; 350];
        let address = game::module_base(instance.pid, path)?
            .checked_add(WRITER_RVA)
            .ok_or_else(|| io::Error::other("Invalid stat writer address."))?;
        self.read(address, &mut loaded)?;
        if loaded != expected || format!("{:x}", Sha256::digest(expected)) != WRITER_HASH {
            return Err(io::Error::other(
                "The loaded stat writer did not match the verified executable.",
            ));
        }
        Ok(())
    }

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
    ) -> io::Result<Vec<(usize, usize, bool)>> {
        let mut matching = Vec::new();
        self.scan_chunks(cancel, |from, bytes, start, end| {
            for offset in candidate_offsets(bytes, start, end) {
                check_cancel(cancel)?;
                for (header, _, length) in name_headers(&bytes[..offset]) {
                    if length == character.len()
                        && self
                            .actor_name(from + header)
                            .is_ok_and(|name| name.eq_ignore_ascii_case(character))
                    {
                        matching.push((from + offset, from + header, false));
                        if !known_build {
                            matching.push((from + offset, from + header, true));
                        }
                        break;
                    }
                }
            }
            Ok(())
        })?;
        Ok(matching)
    }

    fn native_snapshot(
        &self,
        cancel: &AtomicBool,
        known_build: bool,
        distances: &[usize],
    ) -> io::Result<(Vec<Actor>, Vec<NativeStats>)> {
        let mut actors = Vec::new();
        let mut packets = Vec::new();
        self.scan_chunks(cancel, |from, bytes, start, end| {
            for offset in candidate_offsets(bytes, start, end) {
                check_cancel(cancel)?;
                for header in distances
                    .iter()
                    .filter_map(|distance| offset.checked_sub(*distance))
                {
                    let header = from + header;
                    if self.actor_name(header).is_err() {
                        continue;
                    }
                    for &level_after_stats in if known_build {
                        &[false][..]
                    } else {
                        &[false, true][..]
                    } {
                        if let Ok(actor) =
                            self.actor_stats(from + offset, header, level_after_stats)
                        {
                            actors.push(actor);
                        }
                    }
                }
            }
            for (offset, _) in bytes[start..end]
                .iter()
                .enumerate()
                .filter(|(_, byte)| **byte == b'A')
            {
                let packet = &bytes[start + offset..];
                if packet.starts_with(STAT_PACKET) {
                    if let Some(stats) = native_stats(packet)
                        && !packets.contains(&stats)
                    {
                        packets.push(stats);
                    }
                }
            }
            Ok(())
        })?;
        Ok((actors, packets))
    }

    fn verify_native_actor(
        &self,
        cancel: &AtomicBool,
        known_build: bool,
        distances: &[usize],
    ) -> io::Result<(Actor, String)> {
        let deadline = Instant::now() + Duration::from_secs(35);
        let mut previous = HashMap::new();
        loop {
            let (actors, packets) = self.native_snapshot(cancel, known_build, distances)?;
            if let Some(selected) = native_actor(&previous, &actors, &packets) {
                let name = self.actor_name(selected.header)?;
                let current = self.actor(selected, &name)?;
                if packets.iter().any(|packet| packet.matches(current)) {
                    return Ok((current, name));
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(
                    "Waiting for one live character to match the native stat feed. Gain experience in a solo run.",
                ));
            }
            previous = actor_samples(actors);
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn scan_chunks(
        &self,
        cancel: &AtomicBool,
        mut visit: impl FnMut(usize, &[u8], usize, usize) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut next = 0;
        let mut buffer = vec![0; CHUNK + NAME_REACH + MAX_PACKET];
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
                    Ok(())
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
                let to = last.saturating_add(MAX_PACKET).min(end);
                let bytes = &mut buffer[..to - from];
                if self.read(from, bytes).is_err() {
                    continue;
                }
                visit(from, bytes, cursor - from, last - from)?;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct NativeStats {
    health: f32,
    stats: [f32; 4],
}

impl NativeStats {
    fn matches(self, actor: Actor) -> bool {
        (self.health - actor.health).abs() <= 0.51
            && self
                .stats
                .into_iter()
                .zip(actor.stats)
                .all(|(expected, value)| (expected - value).abs() <= 0.0051)
    }
}

fn native_stats(packet: &[u8]) -> Option<NativeStats> {
    let payload = packet.strip_prefix(b"AZS\x09")?;
    let payload = &payload[..payload.len().min(MAX_PACKET)];
    let mut fields = payload.split(|&byte| byte == b';');
    let mut read_field = |key: &str| {
        let (label, value) = std::str::from_utf8(fields.next()?).ok()?.split_once('=')?;
        if label != key {
            return None;
        }
        let value = value.parse::<f32>().ok()?;
        (0.0..=1e6).contains(&value).then_some(value)
    };
    let health = read_field("hp")?;
    let damage = read_field("dmg")?;
    for key in ["cd", "crit", "cm", "arm", "reg"] {
        read_field(key)?;
    }
    let area = read_field("area")?;
    for key in ["cnt", "echo", "spd", "pick", "xp"] {
        read_field(key)?;
    }
    let luck = read_field("luck")?;
    let gold = read_field("gold")?;
    Some(NativeStats {
        health,
        stats: [damage, area, luck, gold],
    })
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

fn native_actor(
    previous: &ActorSamples,
    actors: &[Actor],
    packets: &[NativeStats],
) -> Option<Actor> {
    let mut live = actors.iter().copied().filter(|actor| {
        progressed(previous, *actor) && packets.iter().any(|packet| packet.matches(*actor))
    });
    let actor = live.next()?;
    live.next().is_none().then_some(actor)
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

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn text_section(image: &[u8]) -> Option<(&[u8], usize)> {
    if !image.starts_with(b"MZ") {
        return None;
    }
    let pe = read_u32(image, 0x3c)? as usize;
    if image.get(pe..pe + 4)? != b"PE\0\0" || read_u16(image, pe + 4)? != 0x8664 {
        return None;
    }
    let count = read_u16(image, pe + 6)? as usize;
    let table = pe + 24 + read_u16(image, pe + 20)? as usize;
    for index in 0..count {
        let section = image.get(table + index * 40..table + (index + 1) * 40)?;
        if &section[..8] == b".text\0\0\0" {
            let rva = read_u32(section, 12)? as usize;
            let size = read_u32(section, 16)?.min(read_u32(section, 8)?) as usize;
            let start = read_u32(section, 20)? as usize;
            return Some((image.get(start..start + size)?, rva));
        }
    }
    None
}

// Parse the memory operand of the SSE instructions used by the known stat writer.
fn writer_operand(code: &[u8], start: usize, scalar: bool, opcode: u8) -> Option<(u8, u32, usize)> {
    let mut cursor = start;
    if scalar {
        if *code.get(cursor)? != 0xf3 {
            return None;
        }
        cursor += 1;
    }
    let rex = *code.get(cursor)?;
    let extended = (0x40..=0x4f).contains(&rex);
    cursor += usize::from(extended);
    if code.get(cursor..cursor + 2)? != [0x0f, opcode] {
        return None;
    }
    let mode = *code.get(cursor + 2)?;
    let base = mode & 7;
    if mode & 0xc0 != 0x80 || base == 4 {
        return None;
    }
    let base = base + u8::from(extended && rex & 1 != 0) * 8;
    Some((base, read_u32(code, cursor + 3)?, cursor + 7))
}

fn writer_end(code: &[u8], start: usize) -> Option<(usize, u32)> {
    let (object, anchor, cursor) = writer_operand(code, start, true, 0x5c)?;
    if !(0x100..=0x4000).contains(&anchor) {
        return None;
    }
    let expected = [0, 4, 8, 12, 20, 24, 28, 32, 36, 40, 44, 48, 52];
    let mut offsets = expected.into_iter();
    let mut search = cursor;
    while search < (start + 350).min(code.len()) {
        let scalar = writer_operand(code, search, true, 0x11);
        match scalar.or_else(|| writer_operand(code, search, false, 0x13)) {
            Some((base, displacement, end)) if base == object => {
                let offset = offsets.next()?;
                if anchor.checked_add(offset)? != displacement || scalar.is_none() != (offset == 12)
                {
                    return None;
                }
                if offset == 52 {
                    return Some((end, anchor));
                }
                search = end;
            }
            Some((_, _, end)) => search = end,
            None => search += 1,
        }
    }
    None
}

fn find_writer(code: &[u8], cancel: &AtomicBool) -> io::Result<(usize, usize, u32)> {
    let mut found = None;
    for (index, chunk) in code.chunks(CHUNK).enumerate() {
        check_cancel(cancel)?;
        for (offset, &byte) in chunk.iter().enumerate() {
            if byte == 0xf3 {
                let start = index * CHUNK + offset;
                if let Some((end, stat_base)) = writer_end(code, start)
                    && found.replace((start, end, stat_base)).is_some()
                {
                    return Err(io::Error::other(
                        "Several code locations match the stat writer.",
                    ));
                }
            }
        }
    }
    found.ok_or_else(|| {
        io::Error::other("Could not find the stat writer pattern in this game build.")
    })
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
    fn native_feed_requires_a_unique_progressing_character() {
        let wire = b"AZS\x09hp=928;dmg=3.457;cd=0.533;crit=0.370;cm=3.82;arm=0.390;reg=8.21;area=1.358;cnt=2;echo=0.269;spd=1.577;pick=22.4;xp=1.00;luck=0.00;gold=1.31;heal=0.0;";
        let packet = native_stats(wire).unwrap();
        assert!(native_stats(&wire[4..]).is_none());
        let live = Actor {
            base: 1000,
            header: 200,
            level: 22,
            experience: 70,
            health: 928.4,
            stats: [3.457, 1.358, 0.0, 1.31],
            historical_area: 5.0,
            level_after_stats: false,
        };
        let stale = Actor { base: 2000, ..live };
        let previous = actor_samples(vec![
            Actor {
                experience: 68,
                ..live
            },
            stale,
        ]);
        assert_eq!(
            native_actor(&previous, &[live, stale], &[packet])
                .unwrap()
                .base,
            live.base
        );
        assert!(native_actor(&previous, &[live, stale], &[]).is_none());
        let both = [
            live,
            Actor {
                experience: 71,
                ..stale
            },
        ];
        assert!(native_actor(&previous, &both, &[packet]).is_none());
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

    #[test]
    fn writer_search_accepts_relocated_code_and_rejects_duplicates() {
        let base = 0x660_u32;
        let mut code = vec![0x90; 32];
        code.extend([0xf3, 0x0f, 0x5c, 0x87]);
        code.extend(base.to_le_bytes());
        for offset in [0, 4, 8, 12, 20, 24, 28, 32, 36, 40, 44, 48, 52] {
            if offset == 12 {
                code.extend([0x0f, 0x13, 0x87]);
            } else {
                code.extend([0xf3, 0x0f, 0x11, 0x87]);
            }
            code.extend((base + offset).to_le_bytes());
        }
        let cancel = AtomicBool::new(false);
        assert_eq!(find_writer(&code, &cancel).unwrap(), (32, code.len(), base));
        let duplicate = code.clone();
        code.extend(duplicate);
        assert!(find_writer(&code, &cancel).is_err());
    }
}
