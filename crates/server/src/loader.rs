//! Track loading.
//!
//! The original resolves identifiers synchronously on the request thread
//! (`util/loading.kt`), so N clients asking for the same URL at the same time means N
//! probes of the same URL, each holding a thread. Three changes here:
//!
//! * **Off the async threads.** Probing is blocking work, so it runs on the blocking
//!   pool. A slow remote cannot stall the runtime.
//! * **Single-flight.** Concurrent requests for one identifier share a single load;
//!   the rest wait on its result.
//! * **A short TTL cache.** Sixty seconds, which is long enough to absorb a queue
//!   being filled and short enough that an expiring URL is not served stale.
//!
//! Only successful and empty results are cached. Caching failures would turn one
//! transient network blip into a minute of them, and the original caches nothing at
//! all, so not caching errors stays closer to it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lavalink_protocol::encoded_track::{self, SourceTail};
use lavalink_protocol::player::{Track, TrackInfo};
use lavalink_protocol::{Exception, LoadResult, Playlist, PlaylistInfo, Severity};
use tokio::sync::{broadcast, Semaphore};

use crate::audio::source::{SourceError, SourceLoad, SourceManager, SourcePlaylist, SourceTrack};
use crate::lock;

const CACHE_TTL: Duration = Duration::from_secs(60);

/// How many loads may run at once. Bounded so a burst of `loadtracks` cannot
/// saturate the blocking pool and starve everything else.
const MAX_CONCURRENT_LOADS: usize = 16;

pub struct Loader {
    /// `Arc` rather than `Box` so a manager can be moved onto a blocking thread
    /// without borrowing the loader.
    managers: Vec<Arc<dyn SourceManager>>,
    cache: Mutex<HashMap<String, CacheEntry>>,
    in_flight: Mutex<HashMap<String, broadcast::Sender<LoadResult>>>,
    permits: Arc<Semaphore>,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    result: LoadResult,
    expires_at: Instant,
}

impl Loader {
    pub fn new(managers: Vec<Arc<dyn SourceManager>>) -> Self {
        Self {
            managers,
            cache: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_LOADS)),
        }
    }

    /// The `sourceManagers` list for `/v4/info`.
    pub fn source_names(&self) -> Vec<String> {
        self.managers
            .iter()
            .map(|manager| manager.name().to_owned())
            .collect()
    }

    /// Resolves an identifier. Never fails: loading problems are carried in the
    /// [`LoadResult`], because `loadtracks` answers 200 even for failures.
    pub async fn load(&self, identifier: &str) -> LoadResult {
        if let Some(cached) = self.cached(identifier) {
            return cached;
        }

        // Join an existing load, or become the one that runs it.
        let mut receiver = {
            let mut in_flight = lock(&self.in_flight);
            match in_flight.get(identifier) {
                Some(sender) => Some(sender.subscribe()),
                None => {
                    let (sender, _) = broadcast::channel(1);
                    in_flight.insert(identifier.to_owned(), sender);
                    None
                }
            }
        };

        if let Some(receiver) = receiver.as_mut() {
            return match receiver.recv().await {
                Ok(result) => result,
                // The leader's task died without publishing. Falling through to our
                // own attempt is better than reporting a failure we did not observe.
                Err(_) => self.load_uncached(identifier).await,
            };
        }

        let result = self.load_uncached(identifier).await;

        if !matches!(result, LoadResult::Error(_)) {
            lock(&self.cache).insert(
                identifier.to_owned(),
                CacheEntry {
                    result: result.clone(),
                    expires_at: Instant::now() + CACHE_TTL,
                },
            );
        }

        // Removing before sending is safe: subscribers join under the same lock, so
        // anyone who got a receiver did so before the removal and is still attached.
        if let Some(sender) = lock(&self.in_flight).remove(identifier) {
            let _ = sender.send(result.clone());
        }

        result
    }

    async fn load_uncached(&self, identifier: &str) -> LoadResult {
        let Some(index) = self
            .managers
            .iter()
            .position(|manager| manager.matches(identifier))
        else {
            // No manager claims it. An unsupported source is 200 + "empty", not an
            // error — clients treat "empty" as "try another node or give up", and an
            // error as "something broke".
            return LoadResult::Empty;
        };

        let Ok(_permit) = self.permits.clone().acquire_owned().await else {
            return LoadResult::Error(Exception::new(
                Severity::Suspicious,
                "the server is shutting down",
                "loader closed",
            ));
        };

        let identifier = identifier.to_owned();
        let manager = Arc::clone(&self.managers[index]);

        // A dedicated thread rather than `spawn_blocking`, and the reason is not
        // performance. Blocking-pool threads still carry the runtime in their
        // thread-local context, and `reqwest::blocking` panics outright when it
        // detects one — "cannot drop a runtime in a context where blocking is not
        // allowed". A plain thread does not inherit that context, so the HTTP source
        // works there. Cost is bounded by the permit acquired above.
        let (tx, rx) = tokio::sync::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("loader".to_owned())
            .spawn(move || {
                // The receiver is gone if the caller was cancelled; nothing to do.
                let _ = tx.send(manager.load(&identifier));
            });

        if let Err(error) = spawned {
            return LoadResult::Error(
                SourceError::Internal(format!("could not start a loader thread: {error}"))
                    .to_exception(),
            );
        }

        let result = match rx.await {
            Ok(result) => result,
            // The thread panicked. Report it as ours, and let everything else carry
            // on: one bad load must not take the node with it.
            Err(_) => Err(SourceError::Internal(
                "the loader thread panicked".to_owned(),
            )),
        };

        match result {
            Ok(SourceLoad::Track(track)) => match encode(track) {
                Ok(track) => LoadResult::Track(Box::new(track)),
                Err(error) => LoadResult::Error(error),
            },
            Ok(SourceLoad::Search(tracks)) => {
                match tracks.into_iter().map(encode).collect::<Result<Vec<_>, _>>() {
                    Ok(tracks) => LoadResult::Search(tracks),
                    Err(error) => LoadResult::Error(error),
                }
            }
            Ok(SourceLoad::Playlist(playlist)) => {
                let SourcePlaylist {
                    name,
                    selected_track,
                    tracks,
                } = playlist;
                match tracks.into_iter().map(encode).collect::<Result<Vec<_>, _>>() {
                    Ok(tracks) => LoadResult::Playlist(Playlist {
                        info: PlaylistInfo {
                            name,
                            // A selection past the end would have clients index out
                            // of bounds, so an out-of-range one becomes "none"
                            // rather than being passed on.
                            selected_track: match usize::try_from(selected_track) {
                                Ok(index) if index < tracks.len() => selected_track,
                                _ => -1,
                            },
                        },
                        // We ship no plugins, so this is `{}` exactly as the
                        // original's built-in sources report it.
                        plugin_info: Default::default(),
                        tracks,
                    }),
                    Err(error) => LoadResult::Error(error),
                }
            }
            Err(SourceError::NotFound) => LoadResult::Empty,
            Err(error) => LoadResult::Error(error.to_exception()),
        }
    }

    fn cached(&self, identifier: &str) -> Option<LoadResult> {
        let mut cache = lock(&self.cache);
        let entry = cache.get(identifier)?;
        if entry.expires_at <= Instant::now() {
            cache.remove(identifier);
            return None;
        }
        Some(entry.result.clone())
    }

    /// Decodes an `encodedTrack` for `decodetrack(s)` and for `PATCH` requests that
    /// carry one.
    pub fn decode(&self, encoded: &str) -> Result<Track, Exception> {
        encoded_track::decode(encoded)
            .map(|decoded| decoded.into_track(encoded.to_owned()))
            .map_err(|error| Exception::common(error.to_string(), error.to_string()))
    }
}

/// Turns a resolved source track into a wire track, encoding its identifier blob.
fn encode(track: SourceTrack) -> Result<Track, Exception> {
    let SourceTrack { info, tail } = track;
    encode_info(&info, &tail).map(|encoded| Track::new(encoded, info))
}

fn encode_info(info: &TrackInfo, tail: &SourceTail) -> Result<String, Exception> {
    encoded_track::encode(info, tail).map_err(|error| {
        Exception::fault(
            format!("could not encode the track: {error}"),
            error.to_string(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A manager that counts loads, so single-flight and caching are observable.
    struct Counting {
        loads: Arc<AtomicUsize>,
        outcome: fn() -> Result<SourceLoad, SourceError>,
    }

    fn ok_track() -> Result<SourceLoad, SourceError> {
        Ok(SourceLoad::Track(SourceTrack {
            info: TrackInfo {
                identifier: "https://example.invalid/a.mp3".into(),
                is_seekable: true,
                author: "author".into(),
                length: 1000,
                is_stream: false,
                position: 0,
                title: "title".into(),
                uri: Some("https://example.invalid/a.mp3".into()),
                source_name: "http".into(),
                artwork_url: None,
                isrc: None,
            },
            tail: SourceTail::Probe("mp3".into()),
        }))
    }

    fn failing() -> Result<SourceLoad, SourceError> {
        Err(SourceError::Remote {
            status: 500,
            reason: "Internal Server Error".into(),
        })
    }

    fn missing() -> Result<SourceLoad, SourceError> {
        Err(SourceError::NotFound)
    }

    fn search() -> Result<SourceLoad, SourceError> {
        Ok(SourceLoad::Search(vec![one_track(), one_track()]))
    }

    fn one_track() -> SourceTrack {
        let SourceLoad::Track(track) = ok_track().unwrap() else {
            unreachable!("ok_track returns a track")
        };
        track
    }

    fn playlist() -> Result<SourceLoad, SourceError> {
        Ok(SourceLoad::Playlist(SourcePlaylist {
            name: "Example Playlist".into(),
            selected_track: 1,
            tracks: vec![one_track(), one_track()],
        }))
    }

    /// A selection the client would index out of bounds with.
    fn playlist_overselected() -> Result<SourceLoad, SourceError> {
        Ok(SourceLoad::Playlist(SourcePlaylist {
            name: "Example Playlist".into(),
            selected_track: 7,
            tracks: vec![one_track()],
        }))
    }

    impl SourceManager for Counting {
        fn name(&self) -> &'static str {
            "http"
        }

        fn matches(&self, identifier: &str) -> bool {
            identifier.starts_with("https://")
        }

        fn load(&self, _identifier: &str) -> Result<SourceLoad, SourceError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            (self.outcome)()
        }
    }

    fn loader(outcome: fn() -> Result<SourceLoad, SourceError>) -> (Loader, Arc<AtomicUsize>) {
        let loads = Arc::new(AtomicUsize::new(0));
        let manager = Counting {
            loads: Arc::clone(&loads),
            outcome,
        };
        (Loader::new(vec![Arc::new(manager)]), loads)
    }

    #[tokio::test]
    async fn an_unclaimed_identifier_is_empty_not_an_error() {
        let (loader, loads) = loader(ok_track);
        assert_eq!(loader.load("ytsearch:never gonna").await, LoadResult::Empty);
        assert_eq!(loads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_loaded_track_carries_a_decodable_encoding() {
        let (loader, _) = loader(ok_track);
        let LoadResult::Track(track) = loader.load("https://example.invalid/a.mp3").await else {
            panic!("expected a track");
        };

        assert_eq!(track.info.source_name, "http");
        assert!(track.plugin_info.is_empty());

        let decoded = encoded_track::decode(&track.encoded).unwrap();
        assert_eq!(decoded.info.title, "title");
        assert_eq!(decoded.tail, SourceTail::Probe("mp3".into()));
    }

    #[tokio::test]
    async fn a_search_becomes_a_search_result_with_every_track_encoded() {
        let (loader, _) = loader(search);
        let LoadResult::Search(tracks) = loader.load("https://example.invalid/q").await else {
            panic!("expected a search result");
        };

        assert_eq!(tracks.len(), 2);
        for track in tracks {
            assert!(encoded_track::decode(&track.encoded).is_ok());
        }
    }

    #[tokio::test]
    async fn a_playlist_becomes_a_playlist_result_with_every_track_encoded() {
        let (loader, _) = loader(playlist);
        let LoadResult::Playlist(playlist) = loader.load("https://example.invalid/list").await
        else {
            panic!("expected a playlist result");
        };

        assert_eq!(playlist.info.name, "Example Playlist");
        assert_eq!(playlist.info.selected_track, 1);
        assert_eq!(playlist.tracks.len(), 2);
        for track in &playlist.tracks {
            assert!(encoded_track::decode(&track.encoded).is_ok());
        }
        // We ship no plugins, so this stays `{}` rather than becoming absent.
        assert!(playlist.plugin_info.is_empty());
    }

    /// Passing an out-of-range selection through would have clients index past the
    /// end of an array they were handed.
    #[tokio::test]
    async fn an_out_of_range_selection_becomes_none() {
        let (loader, _) = loader(playlist_overselected);
        let LoadResult::Playlist(playlist) = loader.load("https://example.invalid/list").await
        else {
            panic!("expected a playlist result");
        };
        assert_eq!(playlist.info.selected_track, -1);
    }

    #[tokio::test]
    async fn a_missing_resource_is_empty() {
        let (loader, _) = loader(missing);
        assert_eq!(
            loader.load("https://example.invalid/gone.mp3").await,
            LoadResult::Empty
        );
    }

    #[tokio::test]
    async fn a_remote_failure_is_an_error_result() {
        let (loader, _) = loader(failing);
        let result = loader.load("https://example.invalid/boom.mp3").await;
        let LoadResult::Error(exception) = result else {
            panic!("expected an error result");
        };
        assert_eq!(exception.severity, Severity::Common);
    }

    #[tokio::test]
    async fn a_repeated_load_is_served_from_the_cache() {
        let (loader, loads) = loader(ok_track);
        loader.load("https://example.invalid/a.mp3").await;
        loader.load("https://example.invalid/a.mp3").await;
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn errors_are_not_cached() {
        let (loader, loads) = loader(failing);
        loader.load("https://example.invalid/boom.mp3").await;
        loader.load("https://example.invalid/boom.mp3").await;
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn distinct_identifiers_do_not_share_a_cache_entry() {
        let (loader, loads) = loader(ok_track);
        loader.load("https://example.invalid/a.mp3").await;
        loader.load("https://example.invalid/b.mp3").await;
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_expired_entry_is_reloaded() {
        let (loader, loads) = loader(ok_track);
        loader.load("https://example.invalid/a.mp3").await;

        lock(&loader.cache)
            .get_mut("https://example.invalid/a.mp3")
            .unwrap()
            .expires_at = Instant::now() - Duration::from_secs(1);

        loader.load("https://example.invalid/a.mp3").await;
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn decoding_a_bad_encoding_reports_a_common_error() {
        let (loader, _) = loader(ok_track);
        let error = loader.decode("not base64!!").unwrap_err();
        assert_eq!(error.severity, Severity::Common);
    }
}
