use image::{RgbaImage, imageops::FilterType};
use std::{io, ptr::null_mut, slice};
use windows::{
    Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap},
    Media::Ocr::{OcrEngine, OcrResult},
    Storage::Streams::DataWriter,
    Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, RECT},
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC,
        DeleteObject, SelectObject,
    },
    Storage::Xps::PrintWindow,
    UI::WindowsAndMessaging::{
        EnumWindows, GetClientRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
        IsWindowVisible,
    },
};

#[derive(Default)]
pub struct Reading {
    pub stats: Vec<String>,
}

impl Reading {
    pub fn has_character_panel(&self) -> bool {
        let character_fields = [
            "HP ", "Damage ", "Haste ", "Crit ", "Armor ", "Area ", "Echo ", "Speed ",
        ];
        self.stats
            .iter()
            .filter(|stat| {
                character_fields
                    .iter()
                    .any(|prefix| stat.starts_with(*prefix))
            })
            .take(2)
            .count()
            == 2
    }
}

// Coordinates are fractions of the full client area, as in the legacy reader.
const STAT_PANELS: [[f32; 4]; 2] = [[0.03, 0.44, 0.25, 0.60], [0.22, 0.12, 0.37, 0.36]];

struct Frame {
    pixels: Vec<u8>,
    width: usize,
    height: usize,
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

pub fn read_ocr(pid: u32) -> Result<Reading, String> {
    let frame = capture(pid).map_err(|error| error.to_string())?;
    let _apartment = Apartment::enter().map_err(|error| error.to_string())?;
    let engine =
        OcrEngine::TryCreateFromUserProfileLanguages().map_err(|error| error.to_string())?;
    let stats = read_stat_panels(&engine, frame).map_err(|error| error.to_string())?;
    Ok(Reading { stats })
}

fn read_stat_panels(engine: &OcrEngine, frame: Frame) -> windows::core::Result<Vec<String>> {
    let Frame {
        pixels,
        width,
        height,
    } = frame;
    let image = RgbaImage::from_raw(width as u32, height as u32, pixels)
        .expect("capture returns a packed 32-bit image");
    let max_dimension = OcrEngine::MaxImageDimension()?;
    let mut text = String::new();
    for [left, top, right, bottom] in STAT_PANELS {
        let x = (left * 2.0 * width as f32) as u32;
        let y = (top * height as f32) as u32;
        let w = (right * 2.0 * width as f32) as u32 - x;
        let h = (bottom * height as f32) as u32 - y;
        let panel = image::imageops::crop_imm(&image, x, y, w, h).to_image();
        let scale = (max_dimension / w.max(h)).clamp(1, 3);
        let enlarged = image::imageops::resize(&panel, w * scale, h * scale, FilterType::Lanczos3);
        // Lanczos filters each channel independently. The captured pixels remain BGRA.
        let result = recognize(
            engine,
            enlarged.as_raw(),
            enlarged.width() as usize,
            enlarged.height() as usize,
        )?;
        text.push_str(&stat_rows(&result)?);
    }
    let stats = stats_from_text(&text);
    if !stats.is_empty() {
        return Ok(stats);
    }
    let result = recognize(engine, image.as_raw(), width, height)?;
    let mut stats = stats_from_text(&stat_rows(&result)?);
    if stats.len() < 2 {
        stats.clear();
    }
    Ok(stats)
}

fn recognize(
    engine: &OcrEngine,
    pixels: &[u8],
    width: usize,
    height: usize,
) -> windows::core::Result<OcrResult> {
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
    engine.RecognizeAsync(&bitmap)?.get()
}

fn stat_rows(result: &OcrResult) -> windows::core::Result<String> {
    struct Word {
        x: f32,
        center: f32,
        height: f32,
        text: String,
    }
    let lines = result.Lines()?;
    let mut words = Vec::new();
    for index in 0..lines.Size()? {
        let line = lines.GetAt(index)?.Words()?;
        for index in 0..line.Size()? {
            let word = line.GetAt(index)?;
            let bounds = word.BoundingRect()?;
            words.push(Word {
                x: bounds.X,
                center: bounds.Y + bounds.Height / 2.0,
                height: bounds.Height,
                text: word.Text()?.to_string_lossy(),
            });
        }
    }
    words.sort_by(|a, b| a.center.total_cmp(&b.center));
    let mut text = String::new();
    let mut remaining = words.as_mut_slice();
    while let Some(first) = remaining.first() {
        let end = remaining
            .partition_point(|word| word.center - first.center <= (first.height * 0.5).max(3.0));
        let (row, rest) = remaining.split_at_mut(end);
        row.sort_by(|a, b| a.x.total_cmp(&b.x));
        for word in row {
            text.push_str(&word.text);
            text.push(' ');
        }
        text.push('\n');
        remaining = rest;
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
        "Gold",
        "Experience",
        "Luck",
        "Shielded",
    ];
    let mut values = Vec::new();
    for label in labels {
        let mut matches = text.lines().filter_map(|line| stat_on_row(line, label));
        if let Some(value) = matches.next() {
            if matches.next().is_none() {
                values.push(value);
            }
        }
    }
    values
}

fn stat_on_row(row: &str, label: &str) -> Option<String> {
    let normalized = row.replace(':', " ");
    let words: Vec<_> = normalized.split_whitespace().collect();
    let label_words: Vec<_> = label.split_whitespace().collect();
    for (offset, candidate) in words.windows(label_words.len()).enumerate() {
        if !candidate
            .iter()
            .zip(&label_words)
            .all(|(word, expected)| word.eq_ignore_ascii_case(expected))
        {
            continue;
        }
        let index = offset + label_words.len();
        let displayed = words.get(index)?;
        if label == "Health" {
            let value = displayed.replace(',', "").parse::<u32>().ok()?;
            return (value > 0).then(|| format!("HP {displayed}"));
        }
        let (displayed, next) = if *displayed == "+" {
            (format!("+{}", words.get(index + 1)?), index + 2)
        } else {
            (displayed.to_string(), index + 1)
        };
        let number = match displayed.strip_suffix('%') {
            Some(number) => number,
            None if words.get(next) == Some(&"%") => &displayed,
            None => continue,
        };
        let value = number.parse::<f32>().ok()?;
        return (value.is_finite() && value >= 0.0).then(|| format!("{label} {number}%"));
    }
    None
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
                let mut title = [0u16; 64];
                let len = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) as usize;
                if String::from_utf16_lossy(&title[..len]).eq_ignore_ascii_case("Azeroth Survivors")
                {
                    search.hwnd = hwnd;
                    return 0;
                }
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
            let stride = width as usize * 4;
            let crop_width = width as usize / 2;
            let height = height as usize;
            let source = slice::from_raw_parts(bits.cast::<u8>(), stride * height);
            let mut pixels = Vec::with_capacity(crop_width * height * 4);
            for row in source.chunks_exact(stride) {
                pixels.extend_from_slice(&row[..crop_width * 4]);
            }
            Ok(Frame {
                pixels,
                width: crop_width,
                height,
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
