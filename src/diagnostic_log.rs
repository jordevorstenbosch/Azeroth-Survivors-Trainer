use std::{
    fs,
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

// run.log keeps only the newest entries, so it never grows past this size.
const MAX_BYTES: usize = 10_000;

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static STARTED: OnceLock<Instant> = OnceLock::new();
static WRITE_LOCK: Mutex<()> = Mutex::new(());

pub fn init(path: PathBuf) {
    STARTED.get_or_init(Instant::now);
    let _ = LOG_PATH.set(path);
}

pub fn info(message: &str) {
    write("INFO", message);
}

pub fn warn(message: &str) {
    write("WARN", message);
}

pub fn error(message: &str) {
    write("ERROR", message);
}

fn write(level: &str, message: &str) {
    let Some(path) = LOG_PATH.get() else {
        return;
    };
    let elapsed = STARTED.get_or_init(Instant::now).elapsed().as_secs_f64();
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64);
    let (year, month, day) = civil_date(wall.div_euclid(86_400));
    let seconds = wall.rem_euclid(86_400);
    // Keep each entry on one line so trimming can remove whole entries.
    let message = message.replace(['\r', '\n'], " | ");
    let line = format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC [+{elapsed:.3}s] {level:<5} {message}",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60,
    );
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = append(path, &line);
}

// Converts days since 1970-01-01 to a calendar date.
fn civil_date(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096)
        / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn append(path: &Path, message: &str) -> io::Result<()> {
    let mut log = Vec::with_capacity(MAX_BYTES);
    match fs::File::open(path) {
        Ok(mut file) => {
            let length = file.metadata()?.len();
            let retained = length.saturating_sub(MAX_BYTES as u64);
            file.seek(SeekFrom::Start(retained))?;
            file.take(MAX_BYTES as u64).read_to_end(&mut log)?;
            if retained != 0 {
                discard_partial_line(&mut log, 0);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut start = message.len().saturating_sub(MAX_BYTES - 1);
    while !message.is_char_boundary(start) {
        start += 1;
    }
    let entry = &message.as_bytes()[start..];
    let overflow = (log.len() + entry.len() + 1).saturating_sub(MAX_BYTES);
    if overflow != 0 {
        discard_partial_line(&mut log, overflow);
    }
    log.extend_from_slice(entry);
    log.push(b'\n');

    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temporary, log)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

fn discard_partial_line(log: &mut Vec<u8>, offset: usize) {
    if offset > 0 && log.get(offset - 1) == Some(&b'\n') {
        log.drain(..offset);
    } else if let Some(end) = log
        .get(offset..)
        .and_then(|suffix| suffix.iter().position(|&byte| byte == b'\n'))
    {
        log.drain(..offset + end + 1);
    } else {
        log.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::civil_date;

    #[test]
    fn converts_unix_days_to_calendar_dates() {
        assert_eq!(civil_date(0), (1970, 1, 1));
        assert_eq!(civil_date(19_723), (2024, 1, 1));
        assert_eq!(civil_date(11_016), (2000, 2, 29));
    }
}
