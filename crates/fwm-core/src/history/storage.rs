use super::{HistoryEntry, HistoryRead, MAX_LOG_BYTES, MAX_MESSAGE_BYTES, MAX_RECORD_BYTES};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub fn rotated_path(path: &Path) -> PathBuf {
    path.with_extension("jsonl.1")
}

/// Read at most two bounded log files, from oldest to newest. A crashed final
/// write or malformed record is skipped with a warning, never silently hidden.
pub fn read_history(path: &Path) -> io::Result<HistoryRead> {
    read_history_with_open(path, open_optional)
}

fn read_history_with_open(
    path: &Path,
    mut open: impl FnMut(&Path) -> io::Result<Option<File>>,
) -> io::Result<HistoryRead> {
    let mut result = HistoryRead::default();
    let mut seen = HashSet::new();
    let archived_path = rotated_path(path);
    let mut pair = None;
    for _ in 0..4 {
        let mut active = open(path)?;
        let before = generation(&mut active)?;
        let archived = open(&archived_path)?;
        let mut current = open(path)?;
        if before == generation(&mut current)? {
            pair = Some((active, archived));
            break;
        }
    }
    let (active, archived) = pair.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            "event history kept rotating during the read; retry to obtain an ordered snapshot",
        )
    })?;
    for (file_path, file) in [(archived_path, archived), (path.to_path_buf(), active)] {
        let Some(mut file) = file else {
            continue;
        };
        let length = file.metadata()?.len();
        let clipped = length > MAX_LOG_BYTES;
        if clipped {
            file.seek(SeekFrom::Start(length - MAX_LOG_BYTES))?;
            result.warnings.push(format!(
                "{} exceeded the history size limit; only its last 2 MiB were read",
                file_path.display()
            ));
        }
        let mut contents = Vec::with_capacity(length.min(MAX_LOG_BYTES) as usize);
        file.take(MAX_LOG_BYTES).read_to_end(&mut contents)?;
        let start = if clipped {
            contents
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(contents.len(), |index| index + 1)
        } else {
            0
        };
        let mut skipped = 0usize;
        for line in contents[start..].split_inclusive(|byte| *byte == b'\n') {
            if !line.ends_with(b"\n") || line.len() > MAX_RECORD_BYTES {
                skipped += 1;
                continue;
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice::<HistoryEntry>(line) {
                Ok(entry) => {
                    let cursor = (entry.daemon_instance_id.clone(), entry.event.sequence);
                    if seen.insert(cursor) {
                        result.entries.push(entry);
                    }
                }
                Err(_) => skipped += 1,
            }
        }
        if skipped != 0 {
            result.warnings.push(format!(
                "{}: skipped {skipped} incomplete, malformed, or oversized history record(s)",
                file_path.display()
            ));
        }
    }
    Ok(result)
}

// The first record identifies a generation using its instance and sequence.
// Unlike timestamps, this works across daemon restarts and clock adjustments.
// Bound reads even for malformed files and restore the retained handle's offset.
fn generation(file: &mut Option<File>) -> io::Result<Option<Vec<u8>>> {
    let Some(file) = file else {
        return Ok(None);
    };
    let mut head = Vec::new();
    BufReader::new((&mut *file).take(MAX_RECORD_BYTES as u64)).read_until(b'\n', &mut head)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(Some(head))
}

fn open_optional(path: &Path) -> io::Result<Option<File>> {
    match File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The daemon is the sole writer. Rotate before appending so neither retained
/// file exceeds 2 MiB, even for the last record in a generation.
pub fn append_history(path: &Path, entry: &HistoryEntry) -> io::Result<()> {
    let mut bounded = entry.clone();
    truncate_message(&mut bounded.event.message, MAX_MESSAGE_BYTES);
    let mut line = serde_json::to_vec(&bounded)?;
    // Escaped control characters can use six JSON bytes per input byte. Bound
    // the serialized size as well as the raw message, preserving an explicit
    // truncation marker instead of dropping an otherwise useful event.
    while line.len() + 1 > MAX_RECORD_BYTES && bounded.event.message.len() > 32 {
        let limit = bounded.event.message.len() / 2;
        truncate_message(&mut bounded.event.message, limit);
        line = serde_json::to_vec(&bounded)?;
    }
    line.push(b'\n');
    if line.len() > MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "history metadata exceeds its 8 KiB record budget",
        ));
    }
    let length = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error),
    };
    if length.saturating_add(line.len() as u64 + 1) > MAX_LOG_BYTES {
        let rotated = rotated_path(path);
        match fs::remove_file(&rotated) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        fs::rename(path, rotated)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    write_record(&mut file, &line)
}

fn write_record(file: &mut (impl Read + Seek + Write), line: &[u8]) -> io::Result<()> {
    if file.seek(SeekFrom::End(0))? != 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0u8];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            file.write_all(b"\n")?;
        }
    }
    file.write_all(line)
}

fn truncate_message(message: &mut String, limit: usize) {
    if message.len() <= limit {
        return;
    }
    let mut boundary = limit.saturating_sub(" [truncated]".len());
    while !message.is_char_boundary(boundary) {
        boundary -= 1;
    }
    message.truncate(boundary);
    message.push_str(" [truncated]");
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
