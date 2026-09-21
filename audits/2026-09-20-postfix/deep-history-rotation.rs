// The runner replaces MODULE_PATH with a temporary copy of production storage.rs.
use fwm_core::{history::*, model::EngineEvent};
use std::{io, path::Path, sync::atomic::{AtomicBool, AtomicUsize, Ordering}};
#[path = "MODULE_PATH"]
mod storage;

static ROTATE_ONCE: AtomicBool = AtomicBool::new(true);
static TARGET_ROTATIONS: AtomicUsize = AtomicUsize::new(2);

fn entry(sequence: u64) -> HistoryEntry {
    HistoryEntry::new("one-instance".into(), EngineEvent {
        server_id: None, sequence, timestamp_ms: sequence, forward_id: Some("rule".into()),
        message: "x".repeat(MAX_MESSAGE_BYTES),
    })
}

fn rotate_between_opens(path: &Path) -> io::Result<()> {
    if !ROTATE_ONCE.swap(false, Ordering::SeqCst) { return Ok(()); }
    let target = TARGET_ROTATIONS.load(Ordering::SeqCst);
    if target == 0 { return Ok(()); }
    let mut rotations = 0;
    let mut previous_length = std::fs::metadata(path)?.len();
    for sequence in 2..=2000 {
        storage::append_history(path, &entry(sequence))?;
        let length = std::fs::metadata(path)?.len();
        if length < previous_length { rotations += 1; }
        if rotations == target { return Ok(()); }
        previous_length = length;
    }
    panic!("fixture did not reach two production rotations");
}

fn main() {
    let rotations: usize = std::env::args().nth(1).unwrap_or_else(|| "2".into()).parse().unwrap();
    TARGET_ROTATIONS.store(rotations, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    storage::append_history(&path, &entry(1)).unwrap();
    // Inject the writer scheduling window after the reader opens active but
    // before it opens archived. Production append/rotation logic is unchanged.
    let snapshot = storage::read_history(&path).unwrap();
    assert!(snapshot.warnings.is_empty());
    let seq: Vec<_> = snapshot.entries.iter().map(|entry| entry.event.sequence).collect();
    let regressions: Vec<_> = seq.windows(2).filter(|pair| pair[0] > pair[1])
        .map(|pair| [pair[0], pair[1]]).collect();
    assert_eq!(regressions.len(), usize::from(rotations == 2));
    let highest = *seq.iter().max().unwrap();
    let tail_one = tail(snapshot.entries.clone(), 1)[0].event.sequence;
    assert_eq!(tail_one < highest, rotations == 2);
    let (fresh, gap) = HistoryCursor::default().take_new(&snapshot.entries);
    assert_eq!(fresh.len() < snapshot.entries.len(), rotations == 2);
    // Stable follow-up read is ordered, demonstrating an interleaving-specific error.
    let stable = storage::read_history(&path).unwrap();
    assert!(stable.warnings.is_empty());
    assert!(stable.entries.windows(2).all(|pair| pair[0].event.sequence < pair[1].event.sequence));
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
        "rotations_between_open_handles":rotations, "returned_count":seq.len(),
        "sequence_regressions":regressions,"highest_returned_sequence":highest,
        "tail_one_sequence":tail_one,"cursor_fresh_count":fresh.len(),"cursor_gap":gap,
        "warnings":snapshot.warnings,"stable_read_ordered":true,
        "stable_tail_sequence":stable.entries.last().unwrap().event.sequence,
        "scope":"temporary files; production append/read and cursor; only scheduling hook added"
    })).unwrap());
}
