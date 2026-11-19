#![windows_subsystem = "windows"]
#![deny(unsafe_op_in_unsafe_fn)]

mod diagnostic_log;
mod discovery;
mod game;
mod hud;
mod settings;
mod update;

use settings::Settings;
use std::{
    env,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::mpsc::{self, Receiver, TryRecvError},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::{
        Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute},
        Gdi::{
            CreateFontW, CreateSolidBrush, DC_BRUSH, DEFAULT_GUI_FONT as UI_FONT, DT_CENTER,
            DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawFocusRect, DrawTextW, FillRect,
            GetStockObject, HFONT, SelectObject, SetBkMode, SetDCBrushColor, SetTextColor,
            TRANSPARENT,
        },
    },
    System::{LibraryLoader::GetModuleHandleW, SystemServices::SS_RIGHT},
    UI::{
        Controls::{
            DRAWITEMSTRUCT,
            Dialogs::{GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_PATHMUSTEXIST, OPENFILENAMEW},
            ODS_DISABLED, ODS_FOCUS, ODS_SELECTED,
        },
        HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem},
        Shell::ShellExecuteW,
        WindowsAndMessaging::*,
    },
};

const TITLE: &str = "Azeroth Survivors Trainer";
const REPO: &str = "https://github.com/nelbin4/Azeroth-Survivors-Trainer";
const START: u16 = 100;
const LAUNCH: u16 = 101;
const TALENTS: u16 = 102;
const CHECK_UPDATES: u16 = 103;
const GITHUB: u16 = 104;
const BACK: u16 = 105;
const SAVES: u16 = 106;
const PRIMARY_LAUNCH: u16 = 107;
const STATUS_DOT: u16 = 108;
const READY_DOT: u16 = 109;
const DIVIDER: u16 = 110;
const LOCATE: u16 = 111;
const PROCESS_LOOKUP_LIMIT: u8 = 10;
const BACKGROUND: u32 = 0x242323;
const TEXT: u32 = 0xf7f4f4;
const SECONDARY: u32 = 0x323030;
const WINDOW_STYLE: u32 = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;

fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Panel {
    Main,
    Talents,
}

struct Window {
    settings: Settings,
    panel: Panel,
    children: Vec<HWND>,
    dpi: i32,
    font: Font,
    heading_font: Font,
    watching: bool,
    running: bool,
    game_instance: Option<game::ProcessInstance>,
    process_lookup_misses: u8,
    build: Option<String>,
    discovery: Option<discovery::Job>,
    discovery_message: String,
    next_discovery: Instant,
    reading: hud::Reading,
    capture: Option<(u64, Receiver<Result<hud::Reading, String>>)>,
    scan_generation: u64,
    next_capture: Instant,
    capture_error: Option<String>,
    message: String,
    update_check: Option<Receiver<Result<update::ReleaseStatus, String>>>,
    update_message: String,
}

struct Font(HFONT);

impl Font {
    fn new(dpi: i32, pixels: i32, weight: i32) -> Self {
        let face = wide("Segoe UI");
        Self(unsafe {
            CreateFontW(
                -(pixels * dpi + 48) / 96,
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                1,
                0,
                0,
                5,
                0,
                face.as_ptr(),
            )
        })
    }

    fn handle(&self) -> HFONT {
        if self.0.is_null() {
            unsafe { GetStockObject(UI_FONT).cast() }
        } else {
            self.0
        }
    }
}

impl Drop for Font {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { DeleteObject(self.0.cast()) };
        }
    }
}

impl Window {
    fn scaled(&self, pixels: i32) -> i32 {
        (pixels * self.dpi + 48) / 96
    }

    fn child(
        &mut self,
        parent: HWND,
        class: &str,
        label: &str,
        style: u32,
        rect: [i32; 4],
        id: u16,
    ) -> HWND {
        let [left, top, width, height] = rect.map(|n| self.scaled(n));
        let child = unsafe {
            let child = CreateWindowExW(
                0,
                wide(class).as_ptr(),
                wide(label).as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                left,
                top,
                width,
                height,
                parent,
                id as usize as _,
                GetModuleHandleW(null()),
                null(),
            );
            if !child.is_null() {
                SendMessageW(child, WM_SETFONT, self.font.handle() as usize, 1);
            }
            child
        };
        if !child.is_null() {
            self.children.push(child);
        }
        child
    }

    fn button(&mut self, parent: HWND, label: &str, rect: [i32; 4], id: u16) {
        self.child(
            parent,
            "BUTTON",
            label,
            WS_TABSTOP | BS_OWNERDRAW as u32,
            rect,
            id,
        );
    }

    fn render(&mut self, hwnd: HWND) {
        for child in self.children.drain(..) {
            unsafe { DestroyWindow(child) };
        }
        let height = match self.panel {
            Panel::Main => 296,
            Panel::Talents => 449,
        };
        let mut outer = RECT {
            left: 0,
            top: 0,
            right: self.scaled(450),
            bottom: self.scaled(height),
        };
        unsafe {
            AdjustWindowRectExForDpi(&mut outer, WINDOW_STYLE, 0, 0, self.dpi as u32);
            SetWindowPos(
                hwnd,
                null_mut(),
                0,
                0,
                outer.right - outer.left,
                outer.bottom - outer.top,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        let configured = self.settings.game_path().is_some_and(game::valid_path);
        let status = if !configured {
            "Game not set up yet"
        } else if self.watching && self.running {
            "Discovering stats"
        } else if self.running {
            "Game detected"
        } else if !self.watching {
            "Ready to launch"
        } else {
            "Waiting for the game"
        };
        let waiting_for_panel =
            self.watching && self.running && !self.reading.has_character_panel();
        let description = if !self.message.is_empty() {
            self.message.clone()
        } else if !self.discovery_message.is_empty() {
            self.discovery_message.clone()
        } else if waiting_for_panel {
            "Open the character panel (C) to see visible stats.".into()
        } else if !configured {
            "Locate AzerothSurvivors.exe to begin.".into()
        } else if self.watching {
            format!(
                "{} values detected on the character sheet.",
                self.reading.stats.len()
            )
        } else {
            "Start the watcher to read your character sheet.".into()
        };
        let toggle = if !configured {
            "Locate game"
        } else if self.watching {
            "Stop Watcher"
        } else {
            "Start Watcher"
        };
        self.button(
            hwnd,
            toggle,
            [16, 14, 132, 32],
            if configured { START } else { LOCATE },
        );
        let launch_id = if configured && !self.running {
            PRIMARY_LAUNCH
        } else {
            LAUNCH
        };
        self.child(
            hwnd,
            "BUTTON",
            "Launch Game",
            WS_TABSTOP
                | BS_OWNERDRAW as u32
                | if self.running || !configured {
                    WS_DISABLED
                } else {
                    0
                },
            [158, 14, 130, 32],
            launch_id,
        );
        self.button(hwnd, "Max Talents", [311, 14, 123, 32], TALENTS);
        self.child(hwnd, "STATIC", "", 0, [16, 58, 418, 1], DIVIDER);
        let dot = if self.running { READY_DOT } else { STATUS_DOT };
        self.child(hwnd, "STATIC", "●", 0, [18, 73, 20, 21], dot);
        let heading = self.child(hwnd, "STATIC", status, 0, [43, 70, 390, 26], 0);
        if !heading.is_null() {
            unsafe { SendMessageW(heading, WM_SETFONT, self.heading_font.handle() as usize, 1) };
        }
        self.child(hwnd, "STATIC", &description, 0, [43, 98, 390, 36], 0);

        if !waiting_for_panel {
            let build = self
                .build
                .as_deref()
                .map_or("—", |build| &build[..12])
                .to_owned();
            let mut visible = self.reading.stats.chunks(2);
            let first = visible
                .next()
                .map_or_else(|| "—".into(), |row| row.join("  ·  "));
            let second = visible
                .next()
                .map_or_else(String::new, |row| row.join("  ·  "));
            let rows = [
                ("Game build", build.as_str()),
                ("Visible stats", &first),
                ("", &second),
                ("Stats applied", "0"),
            ];
            for (index, (label, value)) in rows.into_iter().enumerate() {
                let top = 137 + index as i32 * 23;
                self.child(hwnd, "STATIC", label, 0, [18, top, 171, 19], 0);
                self.child(hwnd, "STATIC", value, 0, [201, top, 232, 19], 0);
                self.child(hwnd, "STATIC", "", 0, [16, top + 19, 418, 1], DIVIDER);
            }
        }

        match self.panel {
            Panel::Main => {}
            Panel::Talents => {
                self.child(hwnd, "STATIC", "Max Talents", 0, [18, 245, 135, 22], 0);
                self.button(hwnd, "Back", [352, 241, 81, 27], BACK);
                let cap = format!("Raise talent cap to {}", self.settings.talent_cap());
                self.child(
                    hwnd,
                    "BUTTON",
                    &cap,
                    WS_DISABLED | BS_OWNERDRAW as u32,
                    [18, 277, 165, 27],
                    0,
                );
                self.child(
                    hwnd,
                    "BUTTON",
                    "Restore 51",
                    WS_DISABLED | BS_OWNERDRAW as u32,
                    [192, 277, 95, 27],
                    0,
                );
                let class = format!("Max talents for  {}", self.settings.talent_class());
                self.child(hwnd, "STATIC", &class, 0, [18, 310, 204, 20], 0);
                self.child(
                    hwnd,
                    "BUTTON",
                    "Create save",
                    WS_DISABLED | BS_OWNERDRAW as u32,
                    [18, 334, 145, 25],
                    0,
                );
                self.button(hwnd, "Open Saves folder", [172, 334, 145, 25], SAVES);
                self.child(
                    hwnd,
                    "STATIC",
                    "Talent changes are unavailable in this build.",
                    0,
                    [18, 367, 296, 20],
                    0,
                );
            }
        }
        if !self.update_message.is_empty() {
            let status = self.update_message.clone();
            self.child(hwnd, "STATIC", &status, 0, [18, height - 55, 414, 23], 0);
        }
        let footer = height - 31;
        self.button(hwnd, "GitHub Page", [16, footer, 104, 24], GITHUB);
        self.child(
            hwnd,
            "BUTTON",
            "Check Updates",
            WS_TABSTOP
                | BS_OWNERDRAW as u32
                | if self.update_check.is_some() {
                    WS_DISABLED
                } else {
                    0
                },
            [127, footer, 121, 24],
            CHECK_UPDATES,
        );
        let version = format!("v{}", env!("CARGO_PKG_VERSION"));
        self.child(
            hwnd,
            "STATIC",
            &version,
            SS_RIGHT,
            [288, footer + 3, 146, 21],
            0,
        );
    }

    fn log(&self, message: &str) {
        let path = self.settings.directory.join("trainer.log");
        let _ = diagnostic_log::append(&path, message);
    }

    fn choose_game(&mut self, parent: HWND) -> bool {
        let Some(path) = pick_game(parent) else {
            return false;
        };
        if !game::valid_path(&path) {
            self.message = "Select a valid AzerothSurvivors.exe first.".into();
            return false;
        }
        if let Err(error) = self.settings.set_game_path(&path) {
            self.message = format!("Could not save the selected game. {error}");
            return false;
        }
        self.message.clear();
        self.process_lookup_misses = 0;
        self.set_game_instance(None);
        self.log("Game executable configured.");
        true
    }

    fn set_game_instance(&mut self, instance: Option<game::ProcessInstance>) -> bool {
        if self.game_instance == instance {
            return false;
        }
        self.game_instance = instance;
        self.running = instance.is_some();
        self.build = None;
        self.reset_scan();
        self.log(match instance {
            Some(process) if process.created.is_some() => "Game process identity verified.",
            Some(_) => "Game detected using legacy PID and executable path lookup.",
            None => "Game process no longer identified.",
        });
        true
    }

    fn refresh_process(&mut self) -> bool {
        let Some(path) = self
            .settings
            .game_path()
            .filter(|path| game::valid_path(path))
            .map(Path::to_owned)
        else {
            self.process_lookup_misses = 0;
            return self.set_game_instance(None);
        };
        let instance = match game::running_instance(&path) {
            Ok(Some(instance)) => {
                self.process_lookup_misses = 0;
                Some(instance)
            }
            Ok(None) | Err(_) => {
                if self.process_lookup_misses < PROCESS_LOOKUP_LIMIT {
                    self.process_lookup_misses += 1;
                    if self.process_lookup_misses == PROCESS_LOOKUP_LIMIT {
                        self.log("Process identity lookup missed 10 times. Trying legacy lookup.");
                    }
                }
                if self.process_lookup_misses == PROCESS_LOOKUP_LIMIT {
                    game::legacy_instance(&path).ok().flatten()
                } else {
                    None
                }
            }
        };
        let mut refresh = self.set_game_instance(instance);
        refresh |= self.poll_hud();
        refresh |= self.poll_discovery(&path);
        refresh
    }

    fn poll_discovery(&mut self, path: &Path) -> bool {
        if !self.watching {
            return false;
        }
        let Some(instance) = self.game_instance else {
            return false;
        };
        let update = if instance.created.is_none() {
            Some("Waiting to verify the game process for memory discovery.".into())
        } else if let Some(job) = &self.discovery {
            let report = match job.result.try_recv() {
                Ok(report) => report,
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => Err(std::io::Error::other(
                    "Discovery worker ended unexpectedly.",
                )),
            };
            self.discovery = None;
            self.next_discovery = Instant::now() + Duration::from_secs(3);
            Some(match report {
                Ok(report) => {
                    if self.build.as_ref() != Some(&report.build) {
                        self.log(&format!("Game build SHA-256: {}", report.build));
                        self.build = Some(report.build);
                    }
                    match (report.candidates, report.unreadable_chunks) {
                        (0, 0) => "Searching game memory for a stat record.".into(),
                        (0, _) => "Some memory could not be read. Retrying discovery.".into(),
                        (1, _) => {
                            "Found a possible stat record. Local player is unverified.".into()
                        }
                        (count, _) => format!(
                            "Found {count} possible stat records. Local player is unverified."
                        ),
                    }
                }
                Err(error) => format!("Memory discovery will retry. {error}"),
            })
        } else if Instant::now() >= self.next_discovery {
            match discovery::Job::start(path.to_owned(), instance) {
                Ok(job) => {
                    self.discovery = Some(job);
                    None
                }
                Err(error) => {
                    self.next_discovery = Instant::now() + Duration::from_secs(3);
                    Some(format!("Could not start memory discovery. {error}"))
                }
            }
        } else {
            None
        };
        if let Some(update) = update {
            let changed = self.discovery_message != update;
            if changed {
                self.log(&update);
                self.discovery_message = update;
            }
            return changed;
        }
        false
    }

    fn poll_hud(&mut self) -> bool {
        let mut refreshed = false;
        if let Some((generation, receiver)) = &self.capture {
            let generation = *generation;
            let result = match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Disconnected) => Some(Err("OCR worker ended.".into())),
                Err(TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.capture = None;
                if self.watching && self.scan_generation == generation {
                    match result {
                        Ok(reading) => {
                            refreshed |= self.capture_error.take().is_some()
                                || self.reading.stats != reading.stats;
                            if refreshed && !reading.stats.is_empty() {
                                self.log(&format!("Visible values: {}", reading.stats.join(", ")));
                            }
                            self.reading = reading;
                        }
                        Err(error) if self.capture_error.as_ref() != Some(&error) => {
                            self.log(&format!("Could not read the game window: {error}"));
                            self.capture_error = Some(error);
                            self.reading = hud::Reading::default();
                            refreshed = true;
                        }
                        Err(_) => {}
                    }
                }
            }
        }
        if self.watching && self.capture.is_none() && Instant::now() >= self.next_capture {
            if let Some(process) = self.game_instance {
                let (sender, receiver) = mpsc::sync_channel(1);
                std::thread::spawn(move || {
                    let _ = sender.send(hud::read_ocr(process.pid));
                });
                self.capture = Some((self.scan_generation, receiver));
                self.next_capture = Instant::now() + Duration::from_secs(4);
            }
        }
        refreshed
    }

    fn reset_scan(&mut self) {
        self.scan_generation = self.scan_generation.wrapping_add(1);
        self.discovery = None;
        self.discovery_message.clear();
        self.next_discovery = Instant::now();
        self.reading = hud::Reading::default();
        self.capture_error = None;
        self.next_capture = Instant::now();
    }

    fn start_update_check(&mut self) {
        if self.update_check.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result =
                update::check(env!("CARGO_PKG_VERSION")).map_err(|error| error.to_string());
            let _ = sender.send(result);
        });
        self.update_check = Some(receiver);
        self.update_message = "Checking GitHub for updates…".into();
    }

    fn poll_update(&mut self) -> bool {
        let Some(receiver) = &self.update_check else {
            return false;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err("Update worker ended.".into()),
        };
        self.update_check = None;
        self.update_message = match result {
            Ok(status) if status.newer => {
                format!("Update available {}. See GitHub Page.", status.tag)
            }
            Ok(status) => format!("No newer release. Latest on GitHub is {}.", status.tag),
            Err(error) => {
                self.log(&format!("Update check failed: {error}"));
                "Update check failed. Try again.".into()
            }
        };
        self.log(&self.update_message);
        true
    }

    fn command(&mut self, hwnd: HWND, command: u16) {
        match command {
            START | LOCATE => {
                if !self.settings.game_path().is_some_and(game::valid_path)
                    && !self.choose_game(hwnd)
                {
                    self.render(hwnd);
                    return;
                }
                self.watching = !self.watching;
                self.message.clear();
                self.reset_scan();
                self.log(if self.watching {
                    "Watcher started."
                } else {
                    "Watcher stopped."
                });
            }
            LAUNCH | PRIMARY_LAUNCH => {
                if !self.settings.game_path().is_some_and(game::valid_path)
                    && !self.choose_game(hwnd)
                {
                    self.render(hwnd);
                    return;
                }
                if let Some(path) = self.settings.game_path() {
                    self.message = match game::launch(path) {
                        Ok(true) => "Game launched.".into(),
                        Ok(false) => "Game is already running.".into(),
                        Err(error) => format!("Could not launch the game. {error}"),
                    };
                    self.log(&self.message);
                }
            }
            TALENTS => {
                self.panel = if self.panel == Panel::Talents {
                    Panel::Main
                } else {
                    Panel::Talents
                }
            }
            BACK => self.panel = Panel::Main,
            GITHUB => open(hwnd, REPO),
            CHECK_UPDATES => self.start_update_check(),
            SAVES => {
                if let Some(folder) = self
                    .settings
                    .game_path()
                    .and_then(Path::parent)
                    .map(|dir| dir.join("Saves"))
                    .filter(|path| path.is_dir())
                {
                    open(hwnd, folder.as_os_str());
                } else {
                    self.message = "Saves folder was not found next to the game.".into();
                }
            }
            _ => return,
        }
        self.refresh_process();
        self.render(hwnd);
    }
}

fn draw_button(item: &DRAWITEMSTRUCT) {
    unsafe {
        let disabled = item.itemState & ODS_DISABLED != 0;
        let primary = matches!(item.CtlID as u16, LOCATE | PRIMARY_LAUNCH);
        let pressed = item.itemState & ODS_SELECTED != 0;
        let fill = if disabled {
            0x302e2e
        } else if primary && pressed {
            0x842b64
        } else if primary {
            0xaa3e83
        } else if pressed {
            0x4a4545
        } else {
            SECONDARY
        };
        let brush = CreateSolidBrush(fill);
        FillRect(item.hDC, &item.rcItem, brush);
        DeleteObject(brush);
        let mut label = [0u16; 128];
        let count = GetWindowTextW(item.hwndItem, label.as_mut_ptr(), label.len() as i32);
        SetTextColor(item.hDC, if disabled { 0xaaa5a5 } else { TEXT });
        SetBkMode(item.hDC, TRANSPARENT as i32);
        let font = SendMessageW(item.hwndItem, WM_GETFONT, 0, 0) as HFONT;
        let font = if font.is_null() {
            GetStockObject(UI_FONT)
        } else {
            font.cast()
        };
        let previous_font = SelectObject(item.hDC, font);
        let mut area = item.rcItem;
        DrawTextW(
            item.hDC,
            label.as_ptr(),
            count,
            &mut area,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
        if item.itemState & ODS_FOCUS != 0 {
            area.left += 3;
            area.top += 3;
            area.right -= 3;
            area.bottom -= 3;
            DrawFocusRect(item.hDC, &area);
        }
        SelectObject(item.hDC, previous_font);
    }
}

fn pick_game(hwnd: HWND) -> Option<PathBuf> {
    let mut path = vec![0u16; 32768];
    let filters = wide("Azeroth Survivors\0AzerothSurvivors.exe\0\0");
    let mut spec: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    spec.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    spec.hwndOwner = hwnd;
    spec.lpstrFile = path.as_mut_ptr();
    spec.nMaxFile = path.len() as u32;
    spec.lpstrFilter = filters.as_ptr();
    spec.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST;
    if unsafe { GetOpenFileNameW(&mut spec) } == 0 {
        return None;
    }
    let end = path.iter().position(|&value| value == 0)?;
    Some(std::ffi::OsString::from_wide(&path[..end]).into())
}

fn open(hwnd: HWND, target: impl AsRef<std::ffi::OsStr>) {
    unsafe {
        ShellExecuteW(
            hwnd,
            wide("open").as_ptr(),
            wide(target).as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    };
}

extern "system" fn wndproc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match message {
            WM_NCCREATE => {
                let data = (*(l as *const CREATESTRUCTW)).lpCreateParams;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, data as isize);
                return 1;
            }
            WM_COMMAND if l != 0 => {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Window;
                if let Some(data) = state.as_mut() {
                    // A file dialog can dispatch timer messages while this command runs.
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    data.command(hwnd, w as u16);
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
                }
                return 0;
            }
            WM_DRAWITEM if l != 0 => {
                draw_button(&*(l as *const DRAWITEMSTRUCT));
                return 1;
            }
            WM_CTLCOLORSTATIC => {
                let color = match GetDlgCtrlID(l as HWND) as u16 {
                    READY_DOT => 0x83ce84,
                    STATUS_DOT => 0x35c4f5,
                    DIVIDER => {
                        SetDCBrushColor(w as _, 0x474343);
                        return GetStockObject(DC_BRUSH) as LRESULT;
                    }
                    _ => TEXT,
                };
                SetTextColor(w as _, color);
                SetBkMode(w as _, TRANSPARENT as i32);
                return GetClassLongPtrW(hwnd, GCLP_HBRBACKGROUND) as LRESULT;
            }
            WM_TIMER => {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Window;
                if let Some(state) = state.as_mut() {
                    let process_changed = state.refresh_process();
                    let update_changed = state.poll_update();
                    if process_changed || update_changed {
                        state.render(hwnd);
                    }
                }
                return 0;
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                return 0;
            }
            _ => {}
        }
        DefWindowProcW(hwnd, message, w, l)
    }
}

fn run() -> std::io::Result<()> {
    let settings = Settings::load()?;
    unsafe {
        SetProcessDPIAware();
        let dpi = GetDpiForSystem() as i32;
        let mut window = Window {
            settings,
            panel: Panel::Main,
            children: Vec::new(),
            dpi,
            font: Font::new(dpi, 15, 400),
            heading_font: Font::new(dpi, 20, 600),
            watching: env::args_os().any(|arg| arg == "--autostart"),
            running: false,
            game_instance: None,
            process_lookup_misses: 0,
            build: None,
            discovery: None,
            discovery_message: String::new(),
            next_discovery: Instant::now(),
            reading: hud::Reading::default(),
            capture: None,
            scan_generation: 0,
            capture_error: None,
            next_capture: Instant::now(),
            message: String::new(),
            update_check: None,
            update_message: String::new(),
        };
        let instance = GetModuleHandleW(null());
        let class = wide("AzerothSurvivorsTrainerRust");
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hIcon: LoadIconW(instance, 1usize as _),
            hbrBackground: CreateSolidBrush(BACKGROUND),
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        if RegisterClassW(&window_class) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            wide(TITLE).as_ptr(),
            WINDOW_STYLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            320,
            220,
            null_mut(),
            null_mut(),
            instance,
            (&mut window as *mut Window).cast(),
        );
        if hwnd.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        SetWindowTextW(hwnd, wide(TITLE).as_ptr());
        let dark = 1i32;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            (&dark as *const i32).cast(),
            size_of::<i32>() as u32,
        );
        window.log("Rust trainer window opened.");
        window.refresh_process();
        window.render(hwnd);
        ShowWindow(hwnd, SW_SHOWNORMAL);
        SetTimer(hwnd, 1, 1500, None);
        let mut message: MSG = std::mem::zeroed();
        loop {
            let state = GetMessageW(&mut message, null_mut(), 0, 0);
            if state == 0 {
                break;
            }
            if state < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if IsDialogMessageW(hwnd, &message) == 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        unsafe {
            MessageBoxW(
                null_mut(),
                wide(error.to_string()).as_ptr(),
                wide(TITLE).as_ptr(),
                MB_ICONERROR,
            )
        };
        std::process::exit(1);
    }
}
