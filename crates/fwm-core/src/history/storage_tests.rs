use super::*;
use crate::model::EngineEvent;
use std::io::Cursor;

fn entry(sequence: u64, message: &str) -> HistoryEntry {
    HistoryEntry::new(
        "instance".into(),
        EngineEvent {
            context: None,
            server_id: None,
            sequence,
            timestamp_ms: sequence,
            forward_id: Some("rule".into()),
            message: message.into(),
        },
    )
}

fn encoded(entry: &HistoryEntry) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(entry).unwrap();
    bytes.push(b'\n');
    bytes
}

#[test]
fn oversized_history_reads_only_the_retained_tail_and_reports_clipping() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let mut file = File::create(&path).unwrap();
    file.set_len(MAX_LOG_BYTES + 1024).unwrap();
    file.seek(SeekFrom::End(0)).unwrap();
    file.write_all(b"\n").unwrap();
    file.write_all(&encoded(&entry(7, "retained"))).unwrap();
    let result = read_history(&path).unwrap();
    assert_eq!(result.entries.len(), 1);
    assert_eq!(result.entries[0].event.message, "retained");
    assert_eq!(result.warnings.len(), 1);
    assert!(result.warnings[0].contains("exceeded the history size limit"));
}

#[test]
fn oversized_metadata_is_rejected_before_modifying_any_history_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let original = encoded(&entry(1, "preserved"));
    fs::write(&path, &original).unwrap();
    fs::write(rotated_path(&path), &original).unwrap();
    let mut oversized = entry(2, "short message");
    oversized.forward_name = Some("n".repeat(MAX_RECORD_BYTES));
    let error = append_history(&path, &oversized).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("metadata"));
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(fs::read(rotated_path(&path)).unwrap(), original);
}

#[test]
fn utf8_and_json_expansion_are_truncated_without_corrupting_the_record() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    for (index, message) in [
        "转发🔌".repeat(MAX_MESSAGE_BYTES),
        "\0转发\n🔌".repeat(MAX_MESSAGE_BYTES),
    ]
    .iter()
    .enumerate()
    {
        append_history(&path, &entry(index as u64 + 1, message)).unwrap();
    }
    let result = read_history(&path).unwrap();
    assert!(result.warnings.is_empty());
    assert_eq!(result.entries.len(), 2);
    for entry in result.entries {
        assert!(entry.event.message.ends_with(" [truncated]"));
        assert!(entry.event.message.len() <= MAX_MESSAGE_BYTES);
        assert!(encoded(&entry).len() <= MAX_RECORD_BYTES);
        assert!(!entry.event.message.contains('\u{fffd}'));
    }
}

#[test]
fn active_and_archived_read_errors_are_not_misreported_as_empty_history() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    assert!(read_history(&path).unwrap().entries.is_empty());
    fs::create_dir(&path).unwrap();
    assert!(read_history(&path).is_err());
    fs::remove_dir(&path).unwrap();
    append_history(&path, &entry(1, "valid active")).unwrap();
    fs::create_dir(rotated_path(&path)).unwrap();
    assert!(read_history(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), encoded(&entry(1, "valid active")));
}

#[test]
fn append_open_and_metadata_failures_preserve_existing_paths() {
    let directory = tempfile::tempdir().unwrap();
    let missing_parent = directory.path().join("absent/events.jsonl");
    assert!(append_history(&missing_parent, &entry(1, "event")).is_err());
    assert!(!missing_parent.exists());
    let file = directory.path().join("file");
    fs::write(&file, "keep").unwrap();
    assert!(append_history(&file.join("events.jsonl"), &entry(1, "event")).is_err());
    assert_eq!(fs::read_to_string(&file).unwrap(), "keep");
    let path = directory.path().join("directory");
    fs::create_dir(&path).unwrap();
    assert!(append_history(&path, &entry(1, "event")).is_err());
    assert_eq!(fs::read_dir(path).unwrap().count(), 0);
}

#[test]
fn rotation_failure_does_not_discard_the_active_generation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let mut active = File::create(&path).unwrap();
    active.write_all(b"original").unwrap();
    active.set_len(MAX_LOG_BYTES).unwrap();
    let archive = rotated_path(&path);
    fs::create_dir(&archive).unwrap();
    fs::write(archive.join("keep"), "do not remove").unwrap();
    assert!(append_history(&path, &entry(2, "next")).is_err());
    assert_eq!(fs::metadata(&path).unwrap().len(), MAX_LOG_BYTES);
    let mut prefix = [0; 8];
    File::open(&path).unwrap().read_exact(&mut prefix).unwrap();
    assert_eq!(&prefix, b"original");
    assert_eq!(
        fs::read_to_string(archive.join("keep")).unwrap(),
        "do not remove"
    );
}

#[test]
fn oversized_and_invalid_utf8_records_are_skipped_without_hiding_later_events() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let mut data = vec![b'x'; MAX_RECORD_BYTES + 1];
    data.extend_from_slice(b"\n\xff\xfe\n \t\r\n");
    data.extend(encoded(&entry(3, "valid")));
    fs::write(&path, data).unwrap();
    let result = read_history(&path).unwrap();
    assert_eq!(result.entries.len(), 1);
    assert_eq!(result.entries[0].event.sequence, 3);
    assert_eq!(result.warnings.len(), 1);
    assert!(result.warnings[0].contains("skipped 2"));
}

#[test]
fn duplicate_generation_records_across_rotation_are_emitted_only_once() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let mut first = encoded(&entry(1, "first"));
    first.extend(encoded(&entry(2, "shared")));
    fs::write(rotated_path(&path), first).unwrap();
    let mut second = encoded(&entry(2, "shared"));
    second.extend(encoded(&entry(3, "last")));
    fs::write(&path, second).unwrap();
    let result = read_history(&path).unwrap();
    assert!(result.warnings.is_empty());
    assert_eq!(
        result
            .entries
            .iter()
            .map(|event| event.event.sequence)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
}

#[derive(Clone, Copy)]
enum Fault {
    Read,
    Seek,
    WriteAfter(usize),
}
struct FaultStream {
    bytes: Cursor<Vec<u8>>,
    fault: Fault,
    written: usize,
}
impl Read for FaultStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if matches!(self.fault, Fault::Read) {
            return Err(io::Error::other("injected read"));
        }
        self.bytes.read(bytes)
    }
}
impl Seek for FaultStream {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if matches!(self.fault, Fault::Seek) {
            return Err(io::Error::other("injected seek"));
        }
        self.bytes.seek(position)
    }
}
impl Write for FaultStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = match self.fault {
            Fault::WriteAfter(limit) => limit.saturating_sub(self.written),
            _ => usize::MAX,
        };
        if remaining == 0 {
            return Err(io::Error::other("injected write"));
        }
        let count = self.bytes.write(&bytes[..bytes.len().min(remaining)])?;
        self.written += count;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn low_level_read_seek_and_separator_write_errors_are_propagated() {
    for (fault, message) in [
        (Fault::Read, "read"),
        (Fault::Seek, "seek"),
        (Fault::WriteAfter(0), "write"),
    ] {
        let mut stream = FaultStream {
            bytes: Cursor::new(b"partial".to_vec()),
            fault,
            written: 0,
        };
        let error = write_record(&mut stream, &encoded(&entry(2, "next"))).unwrap_err();
        assert!(error.to_string().contains(message));
        assert_eq!(stream.bytes.into_inner(), b"partial");
    }
}

#[test]
fn partial_record_write_reports_failure_and_later_append_recovers() {
    let previous = encoded(&entry(1, "first"));
    let mut stream = FaultStream {
        bytes: Cursor::new(previous.clone()),
        fault: Fault::WriteAfter(5),
        written: 0,
    };
    assert!(write_record(&mut stream, &encoded(&entry(2, "interrupted"))).is_err());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    fs::write(&path, stream.bytes.into_inner()).unwrap();
    append_history(&path, &entry(3, "recovered")).unwrap();
    let result = read_history(&path).unwrap();
    assert_eq!(
        result
            .entries
            .iter()
            .map(|event| event.event.sequence)
            .collect::<Vec<_>>(),
        [1, 3]
    );
    assert_eq!(result.warnings.len(), 1);
    assert!(result.warnings[0].contains("skipped 1"));
    assert!(fs::read(&path).unwrap().starts_with(&previous));
}

#[test]
fn two_rotations_between_opens_retry_to_an_ordered_generation() {
    for rotations in 0..=2 {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        append_history(&path, &entry(1, "first")).unwrap();
        let mut opens = 0;
        let result = read_history_with_open(&path, |requested| {
            opens += 1;
            if opens == 2 {
                let mut remaining = rotations;
                let mut sequence = 2;
                while remaining > 0 {
                    let before = fs::metadata(&path).unwrap().len();
                    append_history(&path, &entry(sequence, &"x".repeat(MAX_MESSAGE_BYTES)))
                        .unwrap();
                    sequence += 1;
                    if fs::metadata(&path).unwrap().len() < before {
                        remaining -= 1;
                    }
                }
            }
            open_optional(requested)
        })
        .unwrap();
        assert!(
            result
                .entries
                .windows(2)
                .all(|pair| pair[0].event.sequence < pair[1].event.sequence)
        );
        let latest = result
            .entries
            .iter()
            .map(|entry| entry.event.sequence)
            .max()
            .unwrap();
        assert_eq!(
            crate::history::tail(result.entries, 1)[0].event.sequence,
            latest
        );
    }
}

#[test]
fn maximum_escaped_ids_and_labels_fit_persisted_record() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let id = "\u{0001}".repeat(128);
    crate::model::validate_id(&id).unwrap();
    assert!(crate::model::validate_id(&format!("{id}x")).is_err());
    let mut record = entry(1, &"\u{0001}".repeat(MAX_MESSAGE_BYTES));
    record.event.forward_id = Some(id.clone());
    record.event.server_id = Some(id.clone());
    record.event.context = Some(crate::model::EventContext {
        forward_name: Some("界".repeat(33)),
        server_id: Some(id.clone()),
        server_name: Some("s".repeat(100)),
        group: Some("g".repeat(100)),
    });
    record.server_id = Some(id);
    record.forward_name = Some("界".repeat(33));
    record.server_name = Some("s".repeat(100));
    record.group = Some("g".repeat(100));
    append_history(&path, &record).unwrap();
    let read = read_history(&path).unwrap();
    assert!(read.warnings.is_empty());
    assert_eq!(read.entries.len(), 1);
    assert!(read.entries[0].event.message.contains("[truncated]"));
    assert!(fs::metadata(path).unwrap().len() <= MAX_RECORD_BYTES as u64);
}

#[test]
fn continuous_rotation_during_validation_returns_a_bounded_retry_error() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    append_history(&path, &entry(1, "initial")).unwrap();
    let mut opens = 0;
    let mut sequence = 1;
    let error = read_history_with_open(&path, |requested| {
        opens += 1;
        if opens % 3 == 0 {
            let archived = rotated_path(&path);
            if archived.exists() {
                fs::remove_file(&archived).unwrap();
            }
            fs::rename(&path, archived).unwrap();
            sequence += 1;
            append_history(&path, &entry(sequence, "rotated")).unwrap();
        }
        open_optional(requested)
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(opens, 12);
    assert_eq!(
        read_history(&path)
            .unwrap()
            .entries
            .last()
            .unwrap()
            .event
            .sequence,
        sequence
    );
}
