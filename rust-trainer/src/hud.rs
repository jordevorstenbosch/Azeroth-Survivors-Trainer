use std::{io, ptr::null_mut, slice};
use windows::{
    Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap},
    Media::Ocr::OcrEngine,
    Storage::Streams::DataWriter,
    Win32::{
        Foundation::HWND as AutomationHwnd,
        System::{
            Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
            WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
        },
        UI::Accessibility::{CUIAutomation, IUIAutomation},
    },
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, RECT},
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC,
        DeleteObject, SelectObject,
    },
    Storage::Xps::PrintWindow,
    UI::WindowsAndMessaging::{
        EnumWindows, GetClientRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    },
};

#[derive(Default)]
pub struct Reading {
    pub name: Option<String>,
    pub stats: Vec<String>,
}

pub const OCR_MISS_LIMIT: u8 = 10;

pub struct Scan {
    pub primary_misses: u8,
    pub reading: Result<Reading, String>,
}

struct Frame {
    pixels: Vec<u8>,
    width: usize,
    height: usize,
}

pub fn scan(pid: u32, previous_misses: u8) -> Scan {
    if let Ok(reading) = read_accessibility(pid) {
        if !reading.stats.is_empty() {
            return Scan {
                primary_misses: 0,
                reading: Ok(reading),
            };
        }
    }
    let primary_misses = previous_misses.saturating_add(1).min(OCR_MISS_LIMIT);
    let reading = if primary_misses == OCR_MISS_LIMIT {
        read_ocr(pid)
    } else {
        Ok(Reading::default())
    };
    Scan {
        primary_misses,
        reading,
    }
}

struct Apartment;

impl Apartment {
    fn enter() -> windows::core::Result<Self> {
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }?;
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

fn read_accessibility(pid: u32) -> windows::core::Result<Reading> {
    let _apartment = Apartment::enter()?;
    let hwnd = game_window(pid);
    // Limit traversal to visible controls within the game window.
    unsafe {
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        let root = automation.ElementFromHandle(AutomationHwnd(hwnd))?;
        let walker = automation.ControlViewWalker()?;
        let mut pending = vec![(root, true)];
        let mut text = String::new();
        for _ in 0..128 {
            let Some((element, is_root)) = pending.pop() else {
                break;
            };
            if element
                .CurrentIsOffscreen()
                .is_ok_and(|value| !value.as_bool())
            {
                if let Ok(name) = element.CurrentName() {
                    text.push_str(&name.to_string());
                    text.push('\n');
                }
            }
            // The root's siblings belong to other windows.
            if !is_root {
                if let Ok(sibling) = walker.GetNextSiblingElement(&element) {
                    pending.push((sibling, false));
                }
            }
            if let Ok(child) = walker.GetFirstChildElement(&element) {
                pending.push((child, false));
            }
        }
        Ok(Reading {
            name: None,
            stats: stats_from_text(&text),
        })
    }
}

fn read_ocr(pid: u32) -> Result<Reading, String> {
    let frame = capture(pid).map_err(|error| error.to_string())?;
    let _apartment = Apartment::enter().map_err(|error| error.to_string())?;
    let engine =
        OcrEngine::TryCreateFromUserProfileLanguages().map_err(|error| error.to_string())?;
    let name = read_name(&engine, &frame);
    let text = recognize(&engine, &frame.pixels, frame.width, frame.height)
        .map_err(|error| error.to_string())?;
    Ok(Reading {
        name,
        stats: stats_from_text(&text),
    })
}

fn read_name(engine: &OcrEngine, frame: &Frame) -> Option<String> {
    // Isolate the left player nameplate from the adjacent target nameplate.
    let left = frame.width * 56 / 1000;
    let right = frame.width * 130 / 1000;
    let top = frame.height * 33 / 1000;
    let bottom = frame.height * 52 / 1000;
    let width = (right - left) * 4;
    let height = (bottom - top) * 4;
    let mut pixels = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            let index = ((top + y / 4) * frame.width + left + x / 4) * 4;
            pixels.extend_from_slice(&frame.pixels[index..index + 4]);
        }
    }
    let text = recognize(engine, &pixels, width, height).ok()?;
    let candidate = text.trim();
    let mut chars = candidate.chars();
    (candidate.len() <= 24
        && candidate.chars().count() >= 3
        && chars.next().is_some_and(char::is_alphabetic)
        && chars.all(|c| c.is_alphanumeric() || c == '-' || c == '\''))
    .then(|| candidate.to_owned())
}

fn recognize(
    engine: &OcrEngine,
    pixels: &[u8],
    width: usize,
    height: usize,
) -> windows::core::Result<String> {
    let writer = DataWriter::new()?;
    writer.WriteBytes(pixels)?;
    let buffer = writer.DetachBuffer()?;
    let bitmap = SoftwareBitmap::CreateWithAlpha(
        BitmapPixelFormat::Bgra8,
        width as i32,
        height as i32,
        BitmapAlphaMode::Ignore,
    )?;
    bitmap.CopyFromBuffer(&buffer)?;
    let result = engine.RecognizeAsync(&bitmap)?.get()?;
    let lines = result.Lines()?;
    let mut text = String::new();
    for index in 0..lines.Size()? {
        text.push_str(&lines.GetAt(index)?.Text()?.to_string_lossy());
        text.push('\n');
    }
    Ok(text)
}

fn stats_from_text(text: &str) -> Vec<String> {
    let labels = [
        "Health",
        "Damage",
        "Haste",
        "Crit Dmg",
        "Crit",
        "Armor",
        "Area",
        "Echo",
        "Speed",
        "Lifesteal",
        "Experience",
        "Luck",
        "Shielded",
    ];
    let mut values = Vec::new();
    for label in labels {
        let mut matches = text.lines().filter_map(|line| {
            let (field, value) = line.trim().split_once(':')?;
            if !field.trim().eq_ignore_ascii_case(label) {
                return None;
            }
            let displayed = value.split_whitespace().next()?;
            if label == "Health" {
                let normalized = displayed.replace(',', "");
                let number = normalized.parse::<u32>().ok()?;
                return (number > 0).then(|| format!("HP {displayed}"));
            }
            let number = displayed.strip_suffix('%')?.parse::<f32>().ok()?;
            (number.is_finite() && number >= 0.0).then(|| format!("{label} {displayed}"))
        });
        if let Some(value) = matches.next() {
            if matches.next().is_none() {
                values.push(value);
            }
        }
    }
    if values.len() < 2 {
        values.clear();
    }
    values
}

fn game_window(pid: u32) -> HWND {
    struct Search {
        pid: u32,
        hwnd: HWND,
    }
    unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> i32 {
        unsafe {
            let search = &mut *(data as *mut Search);
            let mut owner = 0;
            GetWindowThreadProcessId(hwnd, &mut owner);
            if owner == search.pid && IsWindowVisible(hwnd) != 0 && IsIconic(hwnd) == 0 {
                search.hwnd = hwnd;
                return 0;
            }
            1
        }
    }
    unsafe {
        let mut search = Search {
            pid,
            hwnd: null_mut(),
        };
        EnumWindows(Some(visit), (&mut search as *mut Search) as LPARAM);
        search.hwnd
    }
}

fn capture(pid: u32) -> io::Result<Frame> {
    let hwnd = game_window(pid);
    unsafe {
        let mut bounds: RECT = std::mem::zeroed();
        if hwnd.is_null() || GetClientRect(hwnd, &mut bounds) == 0 {
            return Err(io::Error::other("Game window is unavailable."));
        }
        let width = bounds.right - bounds.left;
        let height = bounds.bottom - bounds.top;
        if !(200..=4096).contains(&width) || !(200..=4096).contains(&height) {
            return Err(io::Error::other(
                "Game window size is outside the capture range.",
            ));
        }
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = size_of_val(&info.bmiHeader) as u32;
        info.bmiHeader.biWidth = width;
        info.bmiHeader.biHeight = -height;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let dc = CreateCompatibleDC(null_mut());
        if dc.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut bits = null_mut();
        let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if bitmap.is_null() || bits.is_null() {
            if !bitmap.is_null() {
                DeleteObject(bitmap);
            }
            DeleteDC(dc);
            return Err(io::Error::other("Unable to allocate a game screenshot."));
        }
        let previous = SelectObject(dc, bitmap);
        let frame = if !previous.is_null() && PrintWindow(hwnd, dc, 3) != 0 {
            Ok(Frame {
                pixels: slice::from_raw_parts(bits.cast::<u8>(), (width * height * 4) as usize)
                    .to_vec(),
                width: width as usize,
                height: height as usize,
            })
        } else {
            Err(io::Error::other("The game did not return a screenshot."))
        };
        if !previous.is_null() {
            SelectObject(dc, previous);
        }
        DeleteObject(bitmap);
        DeleteDC(dc);
        frame
    }
}
