use std::time::Duration;

/// The host clipboard, as seen from inside a container.
///
/// Implemented by `cella-agent` against the daemon control channel, and by a
/// stub in tests. Methods are synchronous and are called from the Wayland
/// server thread and from short-lived worker threads, never from a tokio
/// worker — see `ClipboardSource` impl notes in cella-agent.
pub trait ClipboardSource: Send + Sync + 'static {
    /// Mime types currently on the host clipboard, with the `TARGETS`
    /// sentinel already filtered out. Empty means "clipboard is empty".
    ///
    /// # Errors
    ///
    /// Returns [`SourceError::Unavailable`] when the clipboard bridge cannot be
    /// reached, or [`SourceError::Timeout`] when it does not answer in time.
    fn targets(&self) -> Result<Vec<String>, SourceError>;

    /// Bytes for one mime type. `Ok(vec![])` means "nothing of that type".
    ///
    /// # Errors
    ///
    /// Returns [`SourceError::Unavailable`] when the clipboard bridge cannot be
    /// reached, or [`SourceError::Timeout`] when it does not answer in time.
    fn fetch(&self, mime_type: &str) -> Result<Vec<u8>, SourceError>;

    /// Push bytes to the host clipboard, replacing its contents.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError::TooLarge`] when the payload exceeds the shared
    /// clipboard cap, or [`SourceError::Unavailable`] / [`SourceError::Timeout`]
    /// when the bridge cannot be reached.
    fn publish(&self, mime_type: &str, data: &[u8]) -> Result<(), SourceError>;
}

/// Lets a caller keep a handle on a source it has already shared — the server
/// takes ownership, so without this a test or a caller that wants to inspect
/// the source afterwards would have no way to hold onto it.
impl<T: ClipboardSource + ?Sized> ClipboardSource for std::sync::Arc<T> {
    fn targets(&self) -> Result<Vec<String>, SourceError> {
        (**self).targets()
    }

    fn fetch(&self, mime_type: &str) -> Result<Vec<u8>, SourceError> {
        (**self).fetch(mime_type)
    }

    fn publish(&self, mime_type: &str, data: &[u8]) -> Result<(), SourceError> {
        (**self).publish(mime_type, data)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("clipboard bridge unavailable: {0}")]
    Unavailable(String),
    #[error("clipboard request timed out after {0:?}")]
    Timeout(Duration),
    #[error("clipboard payload {actual} bytes exceeds {limit} byte cap")]
    TooLarge { actual: usize, limit: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EmptySource;

    impl ClipboardSource for EmptySource {
        fn targets(&self) -> Result<Vec<String>, SourceError> {
            Ok(Vec::new())
        }
        fn fetch(&self, _mime_type: &str) -> Result<Vec<u8>, SourceError> {
            Ok(Vec::new())
        }
        fn publish(&self, _mime_type: &str, _data: &[u8]) -> Result<(), SourceError> {
            Ok(())
        }
    }

    #[test]
    fn empty_source_reports_no_targets() {
        assert!(EmptySource.targets().unwrap().is_empty());
    }
}
