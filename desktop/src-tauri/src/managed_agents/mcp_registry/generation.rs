//! Staged configuration generations with a durable journal (memo decision 8).
//!
//! Ordering the writes is not enough. Dropping a server from the registry first
//! leaves agents launching from old generated configs that still name it, and
//! adding one exposes a generated config before the record that enables it, so
//! some prefix is always torn and two agents can sit on different generations.
//! One pointer rename is the only write the whole set can share.
//!
//! So every change is staged whole under `generations/<n>/` and adopted by
//! renaming one pointer onto `current`. A journal records the intent in three
//! phases, each fsynced before the next step begins: `PREPARED` before the
//! first write, `FLIPPED` immediately after the rename, and the entry is
//! removed only once every post-flip deletion has succeeded (`CLEANED`). A
//! journal cleared at the rename would strand a credential whose keychain
//! delete failed a moment later.
//!
//! One mutation lock is held from the base-generation read through cleanup, so
//! two concurrent Settings actions cannot both stage from generation N: the
//! loser waits, re-reads the new base, and restages. Inside that lock, a
//! commit first finishes whatever the previous one left owed, or refuses —
//! writing a fresh journal over an outstanding one would discard a keychain
//! delete the operator has already been told happened.
//!
//! Recovery reads the durable pointer, not the journal phase alone. The rename
//! and the FLIPPED journal write are two writes, so a crash between them leaves
//! a PREPARED journal naming the generation `current` already resolves to; that
//! is an adopted change one write short of its record, not a half-staged tree,
//! and discarding it would delete the live configuration.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Generations kept on disk: the current one plus one to roll back to.
pub const RETAINED_GENERATIONS: u64 = 2;

/// The most the `current` pointer may hold. A `u64` in decimal is at most 20
/// digits; anything past this is not a generation number, so the read is
/// bounded rather than trusting a file another process could have grown.
pub(super) const MAX_POINTER_BYTES: usize = 64;

/// Phase of an in-flight configuration change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalPhase {
    /// Staging has begun; the pointer has not moved.
    Prepared,
    /// The pointer has moved; post-flip deletions are outstanding.
    Flipped,
}

/// A post-flip deletion that must be retried until it succeeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Deletion {
    /// A secret to remove from the keychain.
    Secret {
        /// The blob key.
        key: String,
    },
    /// A staged generation directory to retire.
    Generation {
        /// Its number.
        number: u64,
    },
}

/// The durable record of an in-flight change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    /// The generation being adopted.
    pub generation: u64,
    /// Its phase.
    pub phase: JournalPhase,
    /// Deletions still owed, in order.
    pub deletions: Vec<Deletion>,
}

/// What one commit will write and then clean up.
#[derive(Debug, Clone, Default)]
pub struct GenerationPlan {
    /// Files to stage, keyed by path relative to the generation directory.
    pub files: Vec<(PathBuf, String)>,
    /// Deletions to run after the flip.
    pub deletions: Vec<Deletion>,
}

/// Removes a secret from the durable store.
///
/// A trait so the generation store can be tested against a store the test
/// controls while running the same retry-until-`CLEANED` code production runs.
pub trait SecretRemover {
    /// Remove `key`. `Ok(())` when it is gone, including when it never existed.
    ///
    /// # Errors
    /// A message when the store could not be reached; the journal keeps the
    /// deletion and the next start retries it.
    fn remove(&self, key: &str) -> Result<(), String>;
}

/// A remover that never fails. Used where a plan has no secret deletions.
pub struct NoSecrets;

impl SecretRemover for NoSecrets {
    fn remove(&self, _key: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Named points a test can fail the commit at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlipStep {
    /// The `PREPARED` journal entry has been written and fsynced.
    JournalPrepared,
    /// The staged file at this index has been written and fsynced.
    FileWritten(usize),
    /// The pointer has been renamed onto `current`.
    PointerRenamed,
    /// The `FLIPPED` journal entry has been written and fsynced.
    JournalFlipped,
    /// The deletion at this index has succeeded.
    DeletionDone(usize),
}

/// Failure injection for `generation_flip_is_atomic`.
///
/// Production passes [`NoHooks`], so the tested path and the shipped path are
/// the same code.
pub trait FlipHooks {
    /// Called after each step. `Err` aborts the commit there.
    fn after(&self, step: FlipStep) -> Result<(), String>;
}

/// The production hooks: never fail.
pub struct NoHooks;

impl FlipHooks for NoHooks {
    fn after(&self, _step: FlipStep) -> Result<(), String> {
        Ok(())
    }
}

/// Why a commit or a reconcile failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GenerationError {
    /// A filesystem operation failed.
    #[error("{operation} {path}: {reason}")]
    Io {
        /// What was being done.
        operation: &'static str,
        /// The path involved.
        path: String,
        /// The OS message.
        reason: String,
    },
    /// The journal on disk could not be read.
    #[error("configuration journal is unreadable: {0}")]
    Journal(String),
    /// The `current` pointer exists but does not name a generation.
    #[error("configuration pointer is unreadable: {0}")]
    Pointer(String),
    /// A post-flip deletion is still owed. The journal keeps it; the next start
    /// retries it.
    #[error("configuration change is committed but {outstanding} cleanup step(s) are still owed: {reason}")]
    CleanupPending {
        /// How many deletions remain.
        outstanding: usize,
        /// Why the first of them failed.
        reason: String,
    },
    /// Failure injection fired.
    #[error("injected failure after {step:?}: {reason}")]
    Injected {
        /// The step it fired after.
        step: FlipStep,
        /// The injected message.
        reason: String,
    },
    /// The mutation lock could not be taken.
    #[error("another configuration change is in progress and could not be waited on: {0}")]
    Lock(String),
}

/// A staging tree with a `current` pointer and a journal.
pub struct GenerationStore {
    root: PathBuf,
}

impl GenerationStore {
    /// Open (creating if needed) the staging tree at `root`.
    ///
    /// # Errors
    /// [`GenerationError::Io`] when the tree cannot be created.
    pub fn open(root: &Path) -> Result<Self, GenerationError> {
        std::fs::create_dir_all(root.join("generations")).map_err(|e| GenerationError::Io {
            operation: "create",
            path: root.join("generations").display().to_string(),
            reason: e.to_string(),
        })?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn pointer_path(&self) -> PathBuf {
        self.root.join("current")
    }

    fn journal_path(&self) -> PathBuf {
        self.root.join("journal.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join("mutation.lock")
    }

    /// The directory of generation `number`.
    pub fn generation_dir(&self, number: u64) -> PathBuf {
        self.root.join("generations").join(number.to_string())
    }

    /// The adopted generation, or `None` before the first flip.
    ///
    /// A pointer that exists but does not parse is an error, never `None`.
    /// Read as "no generation" it would restart numbering at 1 over a tree
    /// that already holds generation directories, so a commit would stage into
    /// a live directory and adopt a mixture of two changes. Absence and
    /// corruption are different states and only absence is recoverable by
    /// writing generation 1.
    ///
    /// # Errors
    /// [`GenerationError::Io`] when the pointer exists but cannot be read;
    /// [`GenerationError::Pointer`] when what it holds is not a generation
    /// number — too long, not UTF-8, or not a `u64`.
    pub fn current(&self) -> Result<Option<u64>, GenerationError> {
        let path = self.pointer_path();
        let io = |operation: &'static str, e: std::io::Error| GenerationError::Io {
            operation,
            path: path.display().to_string(),
            reason: e.to_string(),
        };
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io("open", e)),
        };
        // Bounded one byte past the cap, so an oversized pointer is
        // distinguishable from one that exactly fills it, and no more than
        // that is ever read into memory. The exhausted limit is what reports
        // it: widening the bound cannot leave the oversize case unreported.
        let mut buffer = Vec::with_capacity(MAX_POINTER_BYTES + 1);
        let mut bounded = file.take(MAX_POINTER_BYTES as u64 + 1);
        bounded
            .read_to_end(&mut buffer)
            .map_err(|e| io("read", e))?;
        let corrupt =
            |reason: String| GenerationError::Pointer(format!("{}: {reason}", path.display()));
        if bounded.limit() == 0 {
            return Err(corrupt(format!("longer than {MAX_POINTER_BYTES} bytes")));
        }
        let text = std::str::from_utf8(&buffer)
            .map_err(|e| corrupt(format!("not UTF-8: {e}")))?
            .trim();
        text.parse()
            .map(Some)
            .map_err(|e| corrupt(format!("{text:?} is not a generation number: {e}")))
    }

    /// The directory spawn resolves through. Never a staged directory: a spawn
    /// always reads one generation whole.
    ///
    /// # Errors
    /// [`GenerationError::Io`] when the pointer cannot be read.
    pub fn current_dir(&self) -> Result<Option<PathBuf>, GenerationError> {
        Ok(self.current()?.map(|number| self.generation_dir(number)))
    }

    /// Read the journal, if one is outstanding.
    ///
    /// # Errors
    /// [`GenerationError::Journal`] when it exists but does not parse — which
    /// is surfaced rather than treated as "no change in flight", because that
    /// reading would strand every deletion it owed.
    pub fn journal(&self) -> Result<Option<Journal>, GenerationError> {
        match std::fs::read(self.journal_path()) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| GenerationError::Journal(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(GenerationError::Journal(e.to_string())),
        }
    }

    /// Stage and adopt one configuration change.
    ///
    /// `build` runs **inside** the mutation lock, after the base generation has
    /// been read, and is handed that base. A writer that loses the lock race
    /// therefore restages against the new base rather than flipping a plan
    /// computed from a generation that is no longer current.
    ///
    /// # Errors
    /// [`GenerationError`]. A failure before the pointer rename leaves the
    /// previous generation adopted and a `PREPARED` journal the next
    /// [`GenerationStore::reconcile`] discards; a failure after it leaves the
    /// new generation adopted and a journal that keeps every deletion still
    /// owed. [`GenerationError::CleanupPending`] before anything is staged
    /// means a previous change still owes a deletion: this mutation was
    /// refused, not applied, and the next attempt retries the debt first.
    pub fn commit<F>(
        &self,
        build: F,
        secrets: &dyn SecretRemover,
        hooks: &dyn FlipHooks,
    ) -> Result<u64, GenerationError>
    where
        F: FnOnce(Option<u64>, Option<&Path>) -> Result<GenerationPlan, GenerationError>,
    {
        let _lock = MutationLock::acquire(&self.lock_path())?;

        // An outstanding journal is finished before anything is staged. A
        // FLIPPED entry owes a keychain delete for a credential the operator
        // already revoked; overwriting it with a fresh PREPARED entry would
        // destroy the only record of that debt and leave the secret in the
        // keychain with nothing left to retry it. When it cannot be finished,
        // the mutation is refused with the pending reason rather than applied
        // over it.
        self.reconcile_locked(secrets, hooks)?;

        let base = self.current()?;
        let base_dir = base.map(|number| self.generation_dir(number));
        let plan = build(base, base_dir.as_deref())?;
        let next = base.map(|number| number + 1).unwrap_or(1);

        let mut deletions = plan.deletions.clone();
        // Retiring the older directory is itself a journaled step, so the
        // staging tree never holds more than the retained generations.
        if let Some(retire) = next.checked_sub(RETAINED_GENERATIONS) {
            if retire >= 1 && self.generation_dir(retire).exists() {
                deletions.push(Deletion::Generation { number: retire });
            }
        }

        self.write_journal(&Journal {
            generation: next,
            phase: JournalPhase::Prepared,
            deletions: deletions.clone(),
        })?;
        self.inject(hooks, FlipStep::JournalPrepared)?;

        let staged = self.generation_dir(next);
        // A leftover directory from a discarded PREPARED generation must not
        // contribute stale files to this one. A discarded PREPARED generation
        // leaves the pointer where it was, so the next commit recomputes this
        // same number: this removal is the only thing standing between a stale
        // tree and the adopted configuration, and a half-failed one would ship
        // obsolete files to agents. It is therefore propagated, never
        // discarded.
        remove_tree(&staged)?;
        for (index, (relative, contents)) in plan.files.iter().enumerate() {
            write_file_synced(&staged.join(relative), contents)?;
            self.inject(hooks, FlipStep::FileWritten(index))?;
        }
        sync_dir(&staged)?;

        self.rename_pointer(next)?;
        self.inject(hooks, FlipStep::PointerRenamed)?;

        self.write_journal(&Journal {
            generation: next,
            phase: JournalPhase::Flipped,
            deletions: deletions.clone(),
        })?;
        self.inject(hooks, FlipStep::JournalFlipped)?;

        self.run_deletions(deletions, secrets, hooks)?;
        Ok(next)
    }

    /// What a reconcile did.
    ///
    /// # Errors
    /// [`GenerationError`] when the journal is unreadable or a deletion is
    /// still owed.
    pub fn reconcile(
        &self,
        secrets: &dyn SecretRemover,
        hooks: &dyn FlipHooks,
    ) -> Result<Reconciled, GenerationError> {
        let _lock = MutationLock::acquire(&self.lock_path())?;
        self.reconcile_locked(secrets, hooks)
    }

    /// The body of [`GenerationStore::reconcile`], with the mutation lock
    /// already held. [`GenerationStore::commit`] runs it inside its own lock,
    /// so the two paths cannot take the lock twice and deadlock.
    fn reconcile_locked(
        &self,
        secrets: &dyn SecretRemover,
        hooks: &dyn FlipHooks,
    ) -> Result<Reconciled, GenerationError> {
        let Some(journal) = self.journal()? else {
            return Ok(Reconciled::Nothing);
        };
        // The durable pointer, not the journal phase, says whether the change
        // was adopted. A crash between the pointer rename and the FLIPPED
        // journal write leaves a PREPARED journal naming the generation the
        // pointer already resolves to; discarding that tree would delete the
        // adopted configuration and leave `current` naming a path that does
        // not exist.
        let adopted = self.current()? == Some(journal.generation);
        match journal.phase {
            JournalPhase::Prepared if !adopted => {
                // Staged but never adopted: the previous generation is still
                // the current one, so the half-staged tree is discarded whole.
                // The journal is cleared only once the tree is actually gone,
                // so a failed discard is retried rather than forgotten.
                remove_tree(&self.generation_dir(journal.generation))?;
                self.clear_journal()?;
                Ok(Reconciled::DiscardedStaging {
                    generation: journal.generation,
                })
            }
            // Either the journal says FLIPPED, or it says PREPARED and the
            // pointer already names this generation — the same state, reached
            // by a crash one write earlier. Both owe the post-flip deletions.
            JournalPhase::Prepared | JournalPhase::Flipped => {
                let owed = journal.deletions.len();
                self.run_deletions(journal.deletions, secrets, hooks)?;
                Ok(Reconciled::CompletedCleanup {
                    generation: journal.generation,
                    deletions: owed,
                })
            }
        }
    }

    /// Run every deletion, clearing the journal only when all have succeeded.
    fn run_deletions(
        &self,
        deletions: Vec<Deletion>,
        secrets: &dyn SecretRemover,
        hooks: &dyn FlipHooks,
    ) -> Result<(), GenerationError> {
        let mut remaining = Vec::new();
        let mut first_failure = None;
        for (index, deletion) in deletions.iter().enumerate() {
            let outcome = match deletion {
                Deletion::Secret { key } => secrets.remove(key),
                Deletion::Generation { number } => {
                    remove_tree(&self.generation_dir(*number)).map_err(|e| e.to_string())
                }
            };
            match outcome {
                Ok(()) => {
                    if let Err(injected) = self.inject(hooks, FlipStep::DeletionDone(index)) {
                        // The injected failure stands in for a crash right
                        // here: everything after this deletion is still owed.
                        remaining.extend(deletions[index + 1..].iter().cloned());
                        self.write_journal(&Journal {
                            generation: self.current()?.unwrap_or_default(),
                            phase: JournalPhase::Flipped,
                            deletions: remaining,
                        })?;
                        return Err(injected);
                    }
                }
                Err(reason) => {
                    first_failure.get_or_insert(reason);
                    remaining.push(deletion.clone());
                }
            }
        }

        if let Some(reason) = first_failure {
            let outstanding = remaining.len();
            // The journal keeps exactly what is still owed, so the next start
            // retries only that — and it is not cleared until it is empty.
            self.write_journal(&Journal {
                generation: self.current()?.unwrap_or_default(),
                phase: JournalPhase::Flipped,
                deletions: remaining,
            })?;
            return Err(GenerationError::CleanupPending {
                outstanding,
                reason,
            });
        }
        self.clear_journal()
    }

    fn inject(&self, hooks: &dyn FlipHooks, step: FlipStep) -> Result<(), GenerationError> {
        hooks
            .after(step)
            .map_err(|reason| GenerationError::Injected { step, reason })
    }

    fn write_journal(&self, journal: &Journal) -> Result<(), GenerationError> {
        let body =
            serde_json::to_string(journal).map_err(|e| GenerationError::Journal(e.to_string()))?;
        write_file_synced(&self.journal_path(), &body)?;
        sync_dir(&self.root)
    }

    fn clear_journal(&self) -> Result<(), GenerationError> {
        match std::fs::remove_file(self.journal_path()) {
            Ok(()) => sync_dir(&self.root),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(GenerationError::Io {
                operation: "remove",
                path: self.journal_path().display().to_string(),
                reason: e.to_string(),
            }),
        }
    }

    /// The single write the whole change shares.
    fn rename_pointer(&self, number: u64) -> Result<(), GenerationError> {
        let staging = self.root.join("current.next");
        write_file_synced(&staging, &number.to_string())?;
        std::fs::rename(&staging, self.pointer_path()).map_err(|e| GenerationError::Io {
            operation: "rename",
            path: self.pointer_path().display().to_string(),
            reason: e.to_string(),
        })?;
        sync_dir(&self.root)
    }
}

/// The outcome of a reconcile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconciled {
    /// No journal was outstanding.
    Nothing,
    /// A `PREPARED` generation was discarded.
    DiscardedStaging {
        /// The discarded generation.
        generation: u64,
    },
    /// A `FLIPPED` generation's deletions all succeeded.
    CompletedCleanup {
        /// The adopted generation.
        generation: u64,
        /// How many deletions were retried.
        deletions: usize,
    },
}

/// Remove a directory tree, treating "already absent" as success and every
/// other failure as an error the caller must handle.
///
/// A discarded removal is a swallowed failure: a partially removed generation
/// directory leaves stale generated files that a later commit would adopt.
fn remove_tree(path: &Path) -> Result<(), GenerationError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(GenerationError::Io {
            operation: "remove",
            path: path.display().to_string(),
            reason: e.to_string(),
        }),
    }
}

fn write_file_synced(path: &Path, contents: &str) -> Result<(), GenerationError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| GenerationError::Io {
            operation: "create",
            path: parent.display().to_string(),
            reason: e.to_string(),
        })?;
    }
    let mut file = File::create(path).map_err(|e| GenerationError::Io {
        operation: "create",
        path: path.display().to_string(),
        reason: e.to_string(),
    })?;
    file.write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| GenerationError::Io {
            operation: "write",
            path: path.display().to_string(),
            reason: e.to_string(),
        })
}

/// fsync a directory so a rename or a create is durable, not just visible.
fn sync_dir(path: &Path) -> Result<(), GenerationError> {
    #[cfg(unix)]
    {
        let dir = File::open(path).map_err(|e| GenerationError::Io {
            operation: "open",
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        dir.sync_all().map_err(|e| GenerationError::Io {
            operation: "sync",
            path: path.display().to_string(),
            reason: e.to_string(),
        })
    }
    #[cfg(not(unix))]
    {
        // Windows has no directory fsync; the file syncs above carry the
        // durability, and `rename` over an existing name is atomic.
        let _ = path;
        Ok(())
    }
}

/// The one configuration-mutation lock.
struct MutationLock {
    #[allow(dead_code)]
    file: File,
}

impl MutationLock {
    fn acquire(path: &Path) -> Result<Self, GenerationError> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|e| GenerationError::Lock(e.to_string()))?;
        // Blocks until the lock is available: the loser waits and then
        // restages, rather than having its action silently discarded. `fs2`
        // rather than std's `File::lock`, which is 1.89+ and the repo declares
        // a 1.88 MSRV (same reason buzz-agent uses it).
        fs2::FileExt::lock_exclusive(&file).map_err(|e| GenerationError::Lock(e.to_string()))?;
        Ok(Self { file })
    }
}
