//! Bounded write-ahead transactions for park custody and its audit ledger.
//! A pending operation owns the complete old and new park images until both
//! destination files and the journal removal are durably committed.

use std::io::{self, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ledger, park, state_dir};

pub(super) const PENDING_FILE: &str = "pending-operation.json";
pub(super) const RECEIPTS_FILE: &str = "started-event-ids.json";
pub(super) const MAX_RECEIPTS: usize = 250_000;
const MAX_RECEIPT_BYTES: u64 = 17 * 1024 * 1024;
const MAX_OPERATION_BYTES: u64 =
    MAX_RECEIPT_BYTES + 4 * (2 * park::MAX_PARK_BYTES + ledger::MAX_LEDGER_BYTES);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) enum OperationKind {
    Park(Uuid),
    Replay { batches: Vec<Uuid>, replay_id: Uuid },
    Discard(Uuid),
    Update,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Operation {
    pub id: Uuid,
    pub agent: String,
    pub kind: OperationKind,
    pub before: Vec<park::ParkedBatch>,
    pub after: Vec<park::ParkedBatch>,
    pub ledger: Option<Vec<ledger::LedgerRecord>>,
    #[serde(default)]
    pub receipts: Option<Vec<String>>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn receipt_bytes(ids: &[String]) -> io::Result<Vec<u8>> {
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    if ids.len() > MAX_RECEIPTS
        || unique.len() != ids.len()
        || ids
            .iter()
            .any(|id| id.len() != 64 || !id.bytes().all(|c| c.is_ascii_hexdigit()))
    {
        return Err(invalid("invalid or over-cap started-event receipts"));
    }
    let bytes = serde_json::to_vec(ids).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_RECEIPT_BYTES {
        return Err(invalid("started-event receipts exceed byte cap"));
    }
    Ok(bytes)
}

pub(super) fn read_receipts(dir: &Path) -> io::Result<std::collections::HashSet<String>> {
    let file = match state_dir::open_read(&dir.join(RECEIPTS_FILE)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > MAX_RECEIPT_BYTES {
        return Err(invalid("started-event receipts exceed byte cap"));
    }
    let mut bytes = vec![];
    file.take(MAX_RECEIPT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECEIPT_BYTES {
        return Err(invalid("started-event receipts exceed byte cap"));
    }
    let ids: Vec<String> = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    receipt_bytes(&ids)?;
    Ok(ids.into_iter().collect())
}

pub(super) fn replay_floor(dir: &Path, initial: u64) -> io::Result<u64> {
    let path = dir.join("ingress-epoch-floor.json");
    match state_dir::open_read(&path) {
        Ok(file) => {
            let mut bytes = vec![];
            file.take(32).read_to_end(&mut bytes)?;
            let floor: u64 = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            if floor > chrono::Utc::now().timestamp().max(0) as u64 {
                return Err(invalid(
                    "saved ingress floor is in the future; operator reconciliation required",
                ));
            }
            Ok(floor)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            state_dir::write_atomic(
                &path,
                &serde_json::to_vec(&initial).map_err(io::Error::other)?,
            )?;
            Ok(initial)
        }
        Err(error) => Err(error),
    }
}

fn images(op: &Operation, agent: &str) -> io::Result<(Vec<u8>, Option<Vec<u8>>)> {
    if op.agent != agent || op.id.is_nil() {
        return Err(invalid("pending operation identity mismatch"));
    }
    if let Some(ids) = &op.receipts {
        receipt_bytes(ids)?;
    }
    // Validate even the retained old image: an unreadable operation must
    // never be reconciled by simply overwriting the current source files.
    for batch in op.before.iter().chain(&op.after) {
        batch.validate()?;
    }
    let before = park::serialize(&op.before).map_err(io::Error::other)?;
    let after = park::serialize(&op.after).map_err(io::Error::other)?;
    if before.len() as u64 > park::MAX_PARK_BYTES
        || after.len() as u64 > park::MAX_PARK_BYTES
        || op.before.len() > park::MAX_PARKED_TOTAL
        || op.after.len() > park::MAX_PARKED_TOTAL
    {
        return Err(invalid("pending park image exceeds cap"));
    }
    let mut ledger_bytes = Vec::new();
    for record in op.ledger.iter().flatten() {
        if record.agent != agent {
            return Err(invalid("pending ledger identity mismatch"));
        }
        let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
        line.push(b'\n');
        if line.len() > ledger::MAX_LINE_BYTES {
            return Err(invalid("pending ledger record exceeds cap"));
        }
        if ledger_bytes.len() as u64 + line.len() as u64 > ledger::MAX_LEDGER_BYTES {
            return Err(invalid("pending ledger image exceeds cap"));
        }
        ledger_bytes.extend(line);
    }
    Ok((after, op.ledger.as_ref().map(|_| ledger_bytes)))
}

pub(super) fn read(dir: &Path, agent: &str) -> io::Result<Option<Operation>> {
    let path = dir.join(PENDING_FILE);
    let file = match state_dir::open_read(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > MAX_OPERATION_BYTES {
        return Err(invalid("pending operation exceeds byte cap"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_OPERATION_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_OPERATION_BYTES {
        return Err(invalid("pending operation exceeds byte cap"));
    }
    let op: Operation = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    images(&op, agent)?;
    Ok(Some(op))
}

pub(super) fn prepare(dir: &Path, op: &Operation) -> io::Result<()> {
    if dir.join(PENDING_FILE).try_exists()? {
        return Err(io::Error::other(
            "a pending operation must be recovered before another write",
        ));
    }
    images(op, &op.agent)?;
    let bytes = serde_json::to_vec(op).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_OPERATION_BYTES {
        return Err(invalid("pending operation exceeds byte cap"));
    }
    state_dir::write_atomic(&dir.join(PENDING_FILE), &bytes)
}

fn finish_with(
    dir: &Path,
    op: &Operation,
    mut write: impl FnMut(&Path, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    finish_with_boundaries(dir, op, &mut write, |_| Ok(()))
}

fn finish_with_boundaries(
    dir: &Path,
    op: &Operation,
    mut write: impl FnMut(&Path, &[u8]) -> io::Result<()>,
    mut boundary: impl FnMut(u8) -> io::Result<()>,
) -> io::Result<()> {
    let (park_bytes, ledger_bytes) = images(op, &op.agent)?;
    boundary(0)?;
    write(&dir.join(park::PARK_FILE), &park_bytes)?;
    boundary(1)?;
    if let Some(ledger_bytes) = ledger_bytes {
        write(&dir.join(ledger::LEDGER_FILE), &ledger_bytes)?;
    }
    boundary(2)?;
    if let Some(ids) = &op.receipts {
        write(&dir.join(RECEIPTS_FILE), &receipt_bytes(ids)?)?;
    }
    boundary(3)?;
    std::fs::remove_file(dir.join(PENDING_FILE))?;
    boundary(4)?;
    state_dir::sync_dir(dir)?;
    boundary(5)
}

/// Complete an operation already validated and durably prepared in this process.
/// Recovery after an interruption still revalidates the on-disk journal.
pub(super) fn commit_prepared(dir: &Path, op: &Operation) -> io::Result<()> {
    finish_with(dir, op, state_dir::write_atomic)
}

pub(super) fn recover(dir: &Path, agent: &str) -> io::Result<Option<OperationKind>> {
    // Also confirms durability after a previous journal-removal fsync failed.
    state_dir::sync_dir(dir)?;
    let Some(op) = read(dir, agent)? else {
        return Ok(None);
    };
    finish_with(dir, &op, state_dir::write_atomic)?;
    Ok(Some(op.kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operation() -> Operation {
        use crate::{
            queue::{BatchEvent, FlushBatch},
            scope::SessionScope,
        };
        let channel_id = Uuid::new_v4();
        let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "durable client work")
            .sign_with_keys(&nostr::Keys::generate())
            .unwrap();
        let batch = FlushBatch {
            batch_id: Uuid::new_v4(),
            channel_id,
            scope: SessionScope::Conversation { channel_id },
            events: vec![BatchEvent {
                event,
                prompt_tag: "test".into(),
                received_at: std::time::Instant::now(),
            }],
            cancelled_events: vec![],
            cancel_reason: None,
            started: Default::default(),
        };
        let now = chrono::Utc::now();
        let before =
            park::ParkedBatch::from_batch(&batch, park::ParkReason::RetriesExhausted, false, now)
                .unwrap();
        let mut after = before.clone();
        after.needs_review = true;
        after.needs_review_reason = Some("operator review".into());
        let record = ledger::LedgerRecord {
            at: now,
            agent: "test-agent".into(),
            body: ledger::LedgerBody::TurnFinished(ledger::TurnFinished {
                batch_id: batch.batch_id,
                channel_id,
                outcome: ledger::TurnOutcome::Ok,
            }),
        };
        Operation {
            id: Uuid::new_v4(),
            agent: "test-agent".into(),
            kind: OperationKind::Update,
            before: vec![before],
            after: vec![after],
            ledger: Some(vec![record]),
            receipts: Some(vec![batch.events[0].event.id.to_hex()]),
        }
    }

    #[test]
    fn recovery_is_idempotent_after_each_durable_boundary() {
        for fail_at in 0..=5 {
            let dir = tempfile::tempdir().unwrap();
            let op = operation();
            state_dir::write_atomic(
                &dir.path().join(park::PARK_FILE),
                &park::serialize(&op.before).unwrap(),
            )
            .unwrap();
            state_dir::write_atomic(&dir.path().join(ledger::LEDGER_FILE), b"").unwrap();
            prepare(dir.path(), &op).unwrap();
            let result =
                finish_with_boundaries(dir.path(), &op, state_dir::write_atomic, |stage| {
                    if stage == fail_at {
                        Err(io::Error::other("injected crash boundary"))
                    } else {
                        Ok(())
                    }
                });
            assert!(result.is_err());
            if fail_at < 4 {
                assert_eq!(read(dir.path(), &op.agent).unwrap().unwrap().id, op.id);
                assert_eq!(
                    recover(dir.path(), &op.agent).unwrap(),
                    Some(op.kind.clone())
                );
            } else {
                assert_eq!(recover(dir.path(), &op.agent).unwrap(), None);
            }
            assert_eq!(recover(dir.path(), &op.agent).unwrap(), None);
            assert_eq!(
                read_receipts(dir.path()).unwrap(),
                op.receipts.clone().unwrap().into_iter().collect()
            );
            let (expected_park, expected_ledger) = images(&op, &op.agent).unwrap();
            assert_eq!(
                std::fs::read(dir.path().join(park::PARK_FILE)).unwrap(),
                expected_park
            );
            assert_eq!(
                std::fs::read(dir.path().join(ledger::LEDGER_FILE)).unwrap(),
                expected_ledger.unwrap()
            );
            let read = park::ParkFile::open(dir.path()).unwrap();
            assert_eq!(read.batches().len(), 1);
            assert_eq!(
                read.batches()[0].events[0].event.id,
                op.before[0].events[0].event.id
            );
        }
    }

    #[test]
    fn started_receipt_cap_refuses_without_truncating_ids() {
        let mut ids: Vec<_> = (0..MAX_RECEIPTS).map(|n| format!("{n:064x}")).collect();
        assert!(receipt_bytes(&ids).is_ok());
        ids.push(format!("{:064x}", MAX_RECEIPTS));
        assert!(receipt_bytes(&ids).is_err());
        assert_eq!(ids.len(), MAX_RECEIPTS + 1);
    }

    #[test]
    fn pure_custody_update_preserves_audit_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut op = operation();
        op.ledger = None;
        let ledger_path = dir.path().join(ledger::LEDGER_FILE);
        std::fs::write(&ledger_path, b"untouched audit fixture").unwrap();
        prepare(dir.path(), &op).unwrap();
        recover(dir.path(), &op.agent).unwrap();
        assert_eq!(
            std::fs::read(ledger_path).unwrap(),
            b"untouched audit fixture"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pending_symlink_is_rejected_without_touching_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("outside");
        std::fs::write(&target, b"preserve").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join(PENDING_FILE)).unwrap();
        assert!(read(dir.path(), "test-agent").is_err());
        assert!(prepare(dir.path(), &operation()).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"preserve");
    }

    #[test]
    fn corrupt_pending_operation_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PENDING_FILE);
        std::fs::write(&path, b"incomplete operation").unwrap();
        assert!(recover(dir.path(), "test-agent").is_err());
        assert!(prepare(dir.path(), &operation()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"incomplete operation");
    }
}
