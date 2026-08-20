//! Incremental reader for Codex session JSONL files.
//!
//! Session files are append-only in the common case, but Codex can rotate or
//! rewrite them while a process is running.  `JsonlTailer` keeps a byte offset
//! and a partial line, resets safely when the file shrinks/replaces, and
//! de-duplicates records by their stable event id (falling back to a digest).

use std::{
    collections::{HashSet, VecDeque},
    fs::{self, File, Metadata},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::SystemTime,
};

use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::warn;

#[derive(Clone, Debug, PartialEq)]
pub struct JsonlRecord {
    pub key: String,
    pub value: Value,
}

#[derive(Debug, Error)]
pub enum JsonlError {
    #[error("failed to inspect JSONL session {path}: {source}", path = .path.display())]
    Metadata {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to open JSONL session {path}: {source}", path = .path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read JSONL session {path}: {source}", path = .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid JSONL record at byte {offset}: {source}")]
    Json {
        offset: u64,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to watch JSONL session: {0}")]
    Watch(String),
}

#[derive(Debug)]
struct FileFingerprint {
    len: u64,
    modified: Option<SystemTime>,
    prefix: Vec<u8>,
}

impl FileFingerprint {
    fn from_metadata(metadata: &Metadata, prefix: Vec<u8>) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            prefix,
        }
    }

    fn looks_replaced(&self, metadata: &Metadata, current_prefix: &[u8]) -> bool {
        metadata.len() < self.len
            || (!self.prefix.is_empty()
                && current_prefix.get(..self.prefix.len()) != Some(self.prefix.as_slice()))
            || self
                .modified
                .zip(metadata.modified().ok())
                .is_some_and(|(previous, current)| current < previous)
    }
}

/// A polling tailer.  Polling is intentional: it works on networked and
/// sandboxed filesystems where native watcher events can be coalesced or lost.
#[derive(Debug)]
pub struct JsonlTailer {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
    fingerprint: Option<FileFingerprint>,
    seen: HashSet<String>,
    seen_order: VecDeque<String>,
    max_seen: usize,
}

impl JsonlTailer {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            partial: Vec::new(),
            fingerprint: None,
            seen: HashSet::new(),
            seen_order: VecDeque::new(),
            max_seen: 4096,
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn poll(&mut self) -> Result<Vec<JsonlRecord>, JsonlError> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(JsonlError::Metadata {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let mut file = File::open(&self.path).map_err(|source| JsonlError::Open {
            path: self.path.clone(),
            source,
        })?;
        let prefix_len = usize::try_from(metadata.len().min(256)).unwrap_or(256);
        let mut prefix = vec![0; prefix_len];
        file.read_exact(&mut prefix)
            .map_err(|source| JsonlError::Read {
                path: self.path.clone(),
                source,
            })?;
        if self
            .fingerprint
            .as_ref()
            .is_some_and(|fingerprint| fingerprint.looks_replaced(&metadata, &prefix))
        {
            self.reset_position();
        }
        self.fingerprint = Some(FileFingerprint::from_metadata(&metadata, prefix));
        file.seek(SeekFrom::Start(self.offset))
            .map_err(|source| JsonlError::Read {
                path: self.path.clone(),
                source,
            })?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| JsonlError::Read {
                path: self.path.clone(),
                source,
            })?;
        self.offset = self.offset.saturating_add(bytes.len() as u64);
        self.partial.extend(bytes);

        let mut candidates = Vec::new();
        let mut complete_end = 0;
        for (index, byte) in self.partial.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let line = &self.partial[complete_end..index];
            complete_end = index + 1;
            let line = trim_cr(line);
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let offset =
                self.offset.saturating_sub(self.partial.len() as u64) + complete_end as u64;
            let value = match serde_json::from_slice::<Value>(line) {
                Ok(value) => value,
                Err(error) => {
                    // A complete malformed line must never remain in
                    // `partial`: otherwise every poll sees the same poison
                    // record and can spin forever. Skip it, retain subsequent
                    // valid records, and leave a diagnostic in the file log.
                    warn!(
                        path = %self.path.display(),
                        offset,
                        %error,
                        "已跳过损坏的 JSONL 记录"
                    );
                    continue;
                }
            };
            let key = record_key(&value);
            candidates.push(JsonlRecord { key, value });
        }
        if complete_end > 0 {
            self.partial.drain(..complete_end);
        }
        let mut records = Vec::new();
        for record in candidates {
            if self.remember(record.key.clone()) {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Begin observing only future complete lines in the current file.
    pub fn seek_to_end(&mut self) -> Result<(), JsonlError> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.reset_position();
                return Ok(());
            }
            Err(source) => {
                return Err(JsonlError::Metadata {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let mut file = File::open(&self.path).map_err(|source| JsonlError::Open {
            path: self.path.clone(),
            source,
        })?;
        let prefix_len = usize::try_from(metadata.len().min(256)).unwrap_or(256);
        let mut prefix = vec![0; prefix_len];
        file.read_exact(&mut prefix)
            .map_err(|source| JsonlError::Read {
                path: self.path.clone(),
                source,
            })?;
        let partial =
            read_partial_suffix(&mut file, metadata.len()).map_err(|source| JsonlError::Read {
                path: self.path.clone(),
                source,
            })?;
        self.offset = metadata.len();
        self.partial = partial;
        self.fingerprint = Some(FileFingerprint::from_metadata(&metadata, prefix));
        Ok(())
    }

    pub fn reset(&mut self) {
        self.reset_position();
        self.seen.clear();
        self.seen_order.clear();
    }

    fn reset_position(&mut self) {
        self.offset = 0;
        self.partial.clear();
        self.fingerprint = None;
    }

    fn remember(&mut self, key: String) -> bool {
        if !self.seen.insert(key.clone()) {
            return false;
        }
        self.seen_order.push_back(key);
        while self.seen_order.len() > self.max_seen {
            if let Some(old) = self.seen_order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
}

fn read_partial_suffix(file: &mut File, file_len: u64) -> std::io::Result<Vec<u8>> {
    const CHUNK_SIZE: u64 = 8 * 1024;

    let mut position = file_len;
    let mut later_chunks = Vec::<Vec<u8>>::new();
    while position > 0 {
        let start = position.saturating_sub(CHUNK_SIZE);
        let chunk_len = usize::try_from(position - start).unwrap_or(usize::MAX);
        let mut chunk = vec![0; chunk_len];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk)?;
        if let Some(newline) = chunk.iter().rposition(|byte| *byte == b'\n') {
            let mut suffix = chunk[(newline + 1)..].to_vec();
            for later in later_chunks.iter().rev() {
                suffix.extend_from_slice(later);
            }
            return Ok(suffix);
        }
        later_chunks.push(chunk);
        position = start;
    }

    let capacity = usize::try_from(file_len).unwrap_or(usize::MAX);
    let mut suffix = Vec::with_capacity(capacity);
    for chunk in later_chunks.iter().rev() {
        suffix.extend_from_slice(chunk);
    }
    Ok(suffix)
}

fn trim_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn record_key(value: &Value) -> String {
    for field in ["eventId", "event_id", "id", "itemId", "turnId"] {
        if let Some(id) = value.get(field).and_then(Value::as_str) {
            return format!("{field}:{id}");
        }
    }
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

/// Start a best-effort native watcher.  The receiver emits a notification when
/// the path or its parent changes; callers should still call `poll()` because
/// watcher events do not contain complete lines.
pub fn watch_jsonl(path: PathBuf) -> Result<(RecommendedWatcher, mpsc::Receiver<()>), JsonlError> {
    let (sender, receiver) = mpsc::channel(32);
    let callback_sender = sender.clone();
    let mut watcher = RecommendedWatcher::new(
        move |result: notify::Result<Event>| {
            if result.is_ok() {
                let _ = callback_sender.try_send(());
            }
        },
        NotifyConfig::default(),
    )
    .map_err(|error| JsonlError::Watch(error.to_string()))?;
    let watch_path = path.parent().unwrap_or_else(|| Path::new("."));
    watcher
        .watch(watch_path, RecursiveMode::NonRecursive)
        .map_err(|error| JsonlError::Watch(error.to_string()))?;
    Ok((watcher, receiver))
}

/// Parse one or more complete JSONL lines while retaining a partial suffix.
/// This small helper is useful for tests and for callers that already own a
/// stream rather than a file.
#[derive(Debug, Default)]
pub struct JsonlLineParser {
    partial: Vec<u8>,
}

impl JsonlLineParser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, JsonlError> {
        self.partial.extend(bytes);
        let mut values = Vec::new();
        let mut complete_end = 0;
        for (index, byte) in self.partial.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let line = trim_cr(&self.partial[complete_end..index]);
            complete_end = index + 1;
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            values.push(
                serde_json::from_slice(line).map_err(|source| JsonlError::Json {
                    offset: index as u64,
                    source,
                })?,
            );
        }
        if complete_end > 0 {
            self.partial.drain(..complete_end);
        }
        Ok(values)
    }

    #[must_use]
    pub fn partial_len(&self) -> usize {
        self.partial.len()
    }
}

#[cfg(test)]
mod tests {
    use std::{fs::OpenOptions, io::Write};

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn parser_handles_half_lines() {
        let mut parser = JsonlLineParser::new();
        assert!(parser.push(br#"{"id":"a"}"#).unwrap().is_empty());
        assert_eq!(parser.partial_len(), 10);
        let values = parser.push(b"\n{\"id\":\"b\"}\n").unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["id"], "a");
    }

    #[test]
    fn tailer_deduplicates_and_handles_rotation() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(file, "{{\"id\":\"one\",\"value\":1}}").unwrap();
        let mut tailer = JsonlTailer::new(path.clone());
        assert_eq!(tailer.poll().unwrap().len(), 1);
        let mut append = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(append, "{{\"id\":\"one\",\"value\":1}}").unwrap();
        assert!(tailer.poll().unwrap().is_empty());
        drop(append);
        fs::write(&path, b"{\"id\":\"two\",\"value\":2}\n").unwrap();
        assert_eq!(tailer.poll().unwrap()[0].value["id"], "two");
    }

    #[test]
    fn seek_to_end_retains_only_an_existing_partial_line() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        fs::write(&path, b"{\"id\":\"old\"}\n{\"id\":\"partial\"").unwrap();
        let mut tailer = JsonlTailer::new(path.clone());
        tailer.seek_to_end().unwrap();

        let mut append = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(append, "}}").unwrap();
        let records = tailer.poll().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].value["id"], "partial");
    }

    #[test]
    fn malformed_complete_line_is_skipped_without_poisoning_future_polls() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        fs::write(
            &path,
            b"{\"id\":\"first\"}\nnot-json\n{\"id\":\"second\"}\n",
        )
        .unwrap();
        let mut tailer = JsonlTailer::new(path);

        let records = tailer.poll().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].value["id"], "first");
        assert_eq!(records[1].value["id"], "second");
        assert!(tailer.poll().unwrap().is_empty());
    }
}
