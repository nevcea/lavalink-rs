//! Local file source.
//!
//! Off by default in the shipped config, and for good reason: it turns
//! `loadtracks?identifier=…` into arbitrary local file reads for anyone holding the
//! node password. The original has the same property; enabling it is a deployment
//! decision, not ours to second-guess, so the behaviour matches and the default
//! stays off.

use std::path::{Path, PathBuf};

use lavalink_protocol::encoded_track::SourceTail;
use lavalink_protocol::player::TrackInfo;

use super::probe::probe;
use super::{SourceError, SourceLoad, SourceManager, SourceTrack};

#[derive(Debug, Default)]
pub struct LocalSource;

impl LocalSource {
    pub fn new() -> Self {
        Self
    }
}

impl SourceManager for LocalSource {
    fn name(&self) -> &'static str {
        "local"
    }

    fn matches(&self, identifier: &str) -> bool {
        // A URL belongs to another manager even if it happens to name a real path.
        if identifier.contains("://") {
            return false;
        }
        Path::new(identifier).is_file()
    }

    fn load(&self, identifier: &str) -> Result<SourceLoad, SourceError> {
        let path = PathBuf::from(identifier);
        let file = std::fs::File::open(&path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => SourceError::NotFound,
            _ => SourceError::Io(error.to_string()),
        })?;

        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        let probed = probe(Box::new(file), extension.as_deref())?;

        // The file name is the last resort for a title, as it is in the original —
        // an untagged file still has to show something useful in a queue.
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(identifier)
            .to_owned();

        Ok(SourceLoad::Track(SourceTrack {
            info: TrackInfo {
                identifier: identifier.to_owned(),
                // A local file is always seekable in principle; whether the seek is
                // exact depends on the container.
                is_seekable: true,
                author: probed.author.unwrap_or_else(|| "Unknown artist".to_owned()),
                length: probed.duration_ms,
                is_stream: false,
                position: 0,
                title: probed.title.unwrap_or(file_name),
                // Local files have no URL to hand back.
                uri: None,
                source_name: self.name().to_owned(),
                artwork_url: None,
                isrc: probed.isrc,
            },
            tail: SourceTail::Probe(probed.container),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_belong_to_another_manager() {
        let source = LocalSource::new();
        assert!(!source.matches("https://example.invalid/a.mp3"));
        assert!(!source.matches("file:///tmp/a.mp3"));
    }

    #[test]
    fn a_path_that_does_not_exist_does_not_match() {
        let source = LocalSource::new();
        assert!(!source.matches("./definitely-not-here-8f3a.mp3"));
    }

    #[test]
    fn a_directory_does_not_match() {
        let source = LocalSource::new();
        assert!(!source.matches("."));
    }

    #[test]
    fn loading_a_missing_file_is_not_found() {
        let source = LocalSource::new();
        assert!(matches!(
            source.load("./definitely-not-here-8f3a.mp3"),
            Err(SourceError::NotFound)
        ));
    }
}
