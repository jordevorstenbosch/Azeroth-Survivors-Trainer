use std::{
    fs,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

const MAX_BYTES: usize = 10_000;

pub fn append(path: &Path, message: &str) -> io::Result<()> {
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
