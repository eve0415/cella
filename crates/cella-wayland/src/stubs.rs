//! Test doubles for [`ClipboardSource`], shared by the server and dispatch tests.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{ClipboardSource, SourceError};

/// A source that serves a fixed set of mime types and payloads, and records
/// everything published back to it.
pub struct StubSource {
    entries: Vec<(String, Vec<u8>)>,
    available: bool,
    published: Mutex<Vec<(String, Vec<u8>)>>,
}

impl StubSource {
    pub fn new(entries: Vec<(String, Vec<u8>)>) -> Self {
        Self {
            entries,
            available: true,
            published: Mutex::new(Vec::new()),
        }
    }

    pub fn with_targets(mimes: &[&str]) -> Self {
        Self::new(
            mimes
                .iter()
                .map(|m| ((*m).to_string(), format!("payload:{m}").into_bytes()))
                .collect(),
        )
    }

    /// A source whose bridge is down — every call fails with `Unavailable`.
    pub fn unavailable() -> Self {
        Self {
            entries: Vec::new(),
            available: false,
            published: Mutex::new(Vec::new()),
        }
    }

    pub fn published(&self) -> Vec<(String, Vec<u8>)> {
        self.published.lock().unwrap().clone()
    }
}

impl ClipboardSource for StubSource {
    fn targets(&self) -> Result<Vec<String>, SourceError> {
        if !self.available {
            return Err(SourceError::Unavailable("stub is down".to_string()));
        }
        Ok(self.entries.iter().map(|(m, _)| m.clone()).collect())
    }

    fn fetch(&self, mime_type: &str) -> Result<Vec<u8>, SourceError> {
        if !self.available {
            return Err(SourceError::Unavailable("stub is down".to_string()));
        }
        Ok(self
            .entries
            .iter()
            .find(|(m, _)| m == mime_type)
            .map(|(_, d)| d.clone())
            .unwrap_or_default())
    }

    fn publish(&self, mime_type: &str, data: &[u8]) -> Result<(), SourceError> {
        if !self.available {
            return Err(SourceError::Unavailable("stub is down".to_string()));
        }
        self.published
            .lock()
            .unwrap()
            .push((mime_type.to_string(), data.to_vec()));
        Ok(())
    }
}

/// Counts `targets()` calls, so cache behaviour is observable.
#[derive(Default)]
pub struct CountingSource {
    calls: AtomicUsize,
}

impl CountingSource {
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ClipboardSource for CountingSource {
    fn targets(&self) -> Result<Vec<String>, SourceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec!["text/plain".to_string()])
    }

    fn fetch(&self, _mime_type: &str) -> Result<Vec<u8>, SourceError> {
        Ok(Vec::new())
    }

    fn publish(&self, _mime_type: &str, _data: &[u8]) -> Result<(), SourceError> {
        Ok(())
    }
}
