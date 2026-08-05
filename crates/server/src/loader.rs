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

use lavalink_protocol::encoded_track;
use lavalink_protocol::player::Track;
use lavalink_protocol::{Exception, LoadResult, Playlist, PlaylistInfo, Severity};
use tokio::sync::{broadcast, Semaphore};

use crate::audio::source::{SourceError, SourceLoad, SourceManager, SourcePlaylist, SourceTrack};
use crate::lock;

const CACHE_TTL: Duration = Duration::from_secs(60);

/// Hard cap on distinct cached identifiers. `sweep_expired` only runs once per
/// tick, so a burst of distinct identifiers between ticks would otherwise grow
/// the map without bound; this bounds it independently of how often the sweep
/// runs. Past the cap a successful load still returns its result, it just is
/// not cached — correctness is unaffected, the next request for it reloads.
const MAX_CACHE_ENTRIES: usize = 10_000;

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

/// Clears a leader's `in_flight` entry on drop, cancellation included.
///
/// Holds the sender this leader itself registered, so a removal — here or in
/// [`Loader::load`]'s normal-completion path — only ever clears the entry it
/// actually owns. A plain remove-by-key would also work most of the time, but
/// not once a new leader has already replaced this one's entry under the same
/// identifier: `HashMap::remove` cannot distinguish "my entry" from "a
/// different generation's entry that happens to share this key", so it would
/// delete a fresh leader's live sender before it ever sends, forcing its
/// followers to retry as new leaders of their own.
struct LeaderGuard<'a> {
    loader: &'a Loader,
    identifier: &'a str,
    sender: broadcast::Sender<LoadResult>,
}

impl LeaderGuard<'_> {
    fn remove_if_still_ours(&self) {
        let mut in_flight = lock(&self.loader.in_flight);
        if in_flight
            .get(self.identifier)
            .is_some_and(|current| current.same_channel(&self.sender))
        {
            in_flight.remove(self.identifier);
        }
    }
}

impl Drop for LeaderGuard<'_> {
    fn drop(&mut self) {
        self.remove_if_still_ours();
    }
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

        // Join an existing load, or become the one that runs it. A leader that dies
        // without publishing (see the Err arm below) sends every follower back here
        // rather than off to its own load, so single-flight still holds:
        // LeaderGuard's Drop clears the dead entry first, so exactly one follower
        // becomes the new leader and the rest re-subscribe to it.
        loop {
            let mut receiver = None;
            let mut own_sender = None;
            {
                let mut in_flight = lock(&self.in_flight);
                match in_flight.get(identifier) {
                    Some(sender) => receiver = Some(sender.subscribe()),
                    None => {
                        let (sender, _) = broadcast::channel(1);
                        in_flight.insert(identifier.to_owned(), sender.clone());
                        own_sender = Some(sender);
                    }
                }
            }

            if let Some(receiver) = receiver.as_mut() {
                match receiver.recv().await {
                    Ok(result) => return result,
                    // The leader's task died without publishing — a client-side
                    // cancellation, most likely. Retry as if we had just arrived:
                    // becoming the new leader ourselves if nobody beat us to it.
                    Err(_) => continue,
                }
            }

            // No receiver means the match above took the None arm and this task
            // became the leader, inserting own_sender itself.
            let sender = own_sender.expect("the leader branch always sets own_sender");

            // Guarantees the entry above is cleared even if this leader's own
            // future is dropped before load_uncached returns (e.g. an HTTP/2
            // stream reset on client cancellation drops this future mid-await).
            // Without it the broadcast::Sender would stay in in_flight forever
            // with nothing to call .send() on it, and every later caller for this
            // identifier would subscribe and hang in recv().await permanently. A
            // no-op on the normal-return path, where the explicit removal below
            // already clears it.
            let leader_guard = LeaderGuard {
                loader: self,
                identifier,
                sender: sender.clone(),
            };

            let result = self.load_uncached(identifier).await;

            if !matches!(result, LoadResult::Error(_)) {
                let mut cache = lock(&self.cache);
                if cache.len() >= MAX_CACHE_ENTRIES {
                    let now = Instant::now();
                    cache.retain(|_, entry| entry.expires_at > now);
                }
                if cache.len() < MAX_CACHE_ENTRIES {
                    cache.insert(
                        identifier.to_owned(),
                        CacheEntry {
                            result: result.clone(),
                            expires_at: Instant::now() + CACHE_TTL,
                        },
                    );
                }
            }

            // Removing before sending is safe: subscribers join under the same
            // lock, so anyone with a receiver got it before the removal.
            // remove_if_still_ours instead of remove-by-key so this can never
            // delete a different leader's entry — see LeaderGuard's docs.
            leader_guard.remove_if_still_ours();
            let _ = sender.send(result.clone());

            return result;
        }
    }

    async fn load_uncached(&self, identifier: &str) -> LoadResult {
        let Ok(permit) = self.permits.clone().acquire_owned().await else {
            return LoadResult::Error(Exception::new(
                Severity::Suspicious,
                "the server is shutting down",
                "loader closed",
            ));
        };

        let identifier = identifier.to_owned();
        // Cloned, not borrowed, because the thread below outlives this future
        // (see the permit comment). A `Vec` of `Arc`s, so this is a pointer copy
        // per registered manager.
        let managers = self.managers.clone();

        // A dedicated thread rather than spawn_blocking — not for performance.
        // Blocking-pool threads still carry the runtime in thread-local context,
        // and reqwest::blocking panics when it detects one ("cannot drop a runtime
        // in a context where blocking is not allowed"). A plain thread doesn't
        // inherit that context, so the HTTP source works there.
        //
        // The permit moves into the thread instead of staying local: this
        // function is cancelled by dropping its own future (a client-side request
        // cancellation), which would otherwise free the permit while the OS
        // thread — already started, nothing left to cancel it — keeps running the
        // real load. A client that cancels and retries in a loop would then
        // accumulate unbounded background loads while the semaphore reports free
        // slots throughout. Holding the permit for the thread's real lifetime
        // ties the concurrency bound to the work actually running, not to
        // whether anyone still awaits it.
        let (tx, rx) = tokio::sync::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("loader".to_owned())
            .spawn(move || {
                let _permit = permit;
                // Choosing the manager happens here too, not on the caller's
                // runtime thread. `matches` looks pure, and for every other
                // source it is — but `LocalSource::matches` ends in
                // `Path::is_file()`, a stat syscall, and it is reached for any
                // identifier without a `://` that no earlier manager claimed
                // (`ytsearch:`-style prefixes for a disabled source, bare
                // filenames). On a slow or unresponsive mount that stalls a
                // runtime worker, which is the whole reason the load itself was
                // moved off the runtime in the first place.
                //
                // First match wins, as `main.rs::source_managers` orders them.
                let result = managers
                    .iter()
                    .find(|manager| manager.matches(&identifier))
                    .map(|manager| manager.load(&identifier));
                // The receiver is gone if the caller was cancelled; nothing to do.
                let _ = tx.send(result);
            });

        if let Err(error) = spawned {
            return LoadResult::Error(
                SourceError::Internal(format!("could not start a loader thread: {error}"))
                    .to_exception(),
            );
        }

        let result = match rx.await {
            Ok(Some(result)) => result,
            // No manager claims it. An unsupported source is 200 + "empty", not
            // an error — clients treat "empty" as "try another node or give
            // up", and an error as "something broke".
            Ok(None) => return LoadResult::Empty,
            // The thread panicked. Report it as ours, and let everything else
            // carry on: one bad load must not take the node with it.
            Err(_) => Err(SourceError::Internal("the loader thread panicked".to_owned())),
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
                        // We ship no plugins, so this is {} exactly as the
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

    /// Drops every cache entry whose TTL has passed.
    ///
    /// [`Self::cached`] evicts too, but only the one identifier it was asked about,
    /// so an entry nobody looks up a second time is never reached by it. Identifiers
    /// come from the client and most are asked for exactly once — a queue is filled,
    /// played, and never resolved again — so without this the map grows for the life
    /// of the process, holding a full [`LoadResult`] (a playlist's whole `Vec<Track>`,
    /// for a `loadtracks` of one) per identifier ever seen.
    ///
    /// `in_flight` is deliberately left alone. Those entries belong to their leader's
    /// [`LeaderGuard`], which removes them by generation rather than by key; a sweep
    /// clearing that map would drop a live leader's sender out from under the
    /// followers waiting on it.
    pub fn sweep_expired(&self) {
        let now = Instant::now();
        lock(&self.cache).retain(|_, entry| entry.expires_at > now);
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
    ///
    /// A track naming a source manager this node did not register is refused, the
    /// way the original's `decodeTrack` refuses it: it resolves `sourceName`
    /// against its registered `sourceManagers` and hands back null when the name
    /// is absent, which the handler turns into a 400. Without that lookup
    /// `sourceName` is nothing but a string the client chose, and
    /// `StreamOpener::open` dispatches straight off it — so a hand-built
    /// `encodedTrack` naming a source the operator left switched off would still
    /// play, `local` (which opens an arbitrary path off the filesystem) included.
    /// This is the only place a track can enter the node without a source manager
    /// having produced it, so it is the only place the check is needed.
    pub fn decode(&self, encoded: &str) -> Result<Track, Exception> {
        let track = encoded_track::decode(encoded)
            .map(|decoded| decoded.into_track(encoded.to_owned()))
            .map_err(|error| Exception::common(error.to_string(), error.to_string()))?;

        if !self.has_manager(&track.info.source_name) {
            let message = format!(
                "no source manager registered for {}",
                track.info.source_name
            );
            return Err(Exception::common(message.clone(), message));
        }

        Ok(track)
    }

    fn has_manager(&self, source_name: &str) -> bool {
        self.managers
            .iter()
            .any(|manager| manager.name() == source_name)
    }
}

/// Turns a resolved source track into a wire track, encoding its identifier blob.
fn encode(track: SourceTrack) -> Result<Track, Exception> {
    let SourceTrack { info, tail } = track;
    encoded_track::encode(&info, &tail)
        .map_err(|error| {
            Exception::fault(
                format!("could not encode the track: {error}"),
                error.to_string(),
            )
        })
        .map(|encoded| Track::new(encoded, info))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lavalink_protocol::encoded_track::SourceTail;
    use lavalink_protocol::player::TrackInfo;
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
        // We ship no plugins, so this stays {} rather than becoming absent.
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

    /// The leak `sweep_expired` exists for: `cached` only evicts the identifier it
    /// was asked about, so an entry that is never looked up again — the normal fate
    /// of a queue's worth of identifiers — stayed in the map forever.
    #[tokio::test]
    async fn the_sweep_drops_expired_entries_that_are_never_looked_up_again() {
        let (loader, _) = loader(ok_track);
        loader.load("https://example.invalid/a.mp3").await;
        loader.load("https://example.invalid/b.mp3").await;

        lock(&loader.cache)
            .get_mut("https://example.invalid/a.mp3")
            .unwrap()
            .expires_at = Instant::now() - Duration::from_secs(1);

        loader.sweep_expired();

        let cache = lock(&loader.cache);
        assert!(!cache.contains_key("https://example.invalid/a.mp3"));
        // Unexpired entries are untouched — the sweep is expiry, not a flush.
        assert!(cache.contains_key("https://example.invalid/b.mp3"));
    }

    /// Distinct identifiers past `MAX_CACHE_ENTRIES` must not grow the cache
    /// without bound, even between sweep ticks.
    #[tokio::test]
    async fn the_cache_does_not_grow_past_its_cap() {
        let (loader, _) = loader(ok_track);
        for entry in 0..MAX_CACHE_ENTRIES + 10 {
            lock(&loader.cache).insert(
                format!("https://example.invalid/{entry}"),
                CacheEntry {
                    result: LoadResult::Empty,
                    expires_at: Instant::now() + CACHE_TTL,
                },
            );
        }
        assert_eq!(lock(&loader.cache).len(), MAX_CACHE_ENTRIES + 10);

        // One more successful load past the cap is served but not cached.
        loader.load("https://example.invalid/one-more").await;
        assert!(
            !lock(&loader.cache).contains_key("https://example.invalid/one-more"),
            "a load past the cache cap must not be cached"
        );
    }

    #[test]
    fn decoding_a_bad_encoding_reports_a_common_error() {
        let (loader, _) = loader(ok_track);
        let error = loader.decode("not base64!!").unwrap_err();
        assert_eq!(error.severity, Severity::Common);
    }

    #[test]
    fn a_registered_source_decodes() {
        let (loader, _) = loader(ok_track);
        let encoded = encode(one_track()).unwrap().encoded;
        assert_eq!(loader.decode(&encoded).unwrap().info.source_name, "http");
    }

    /// The bug: nothing checked `sourceName` against the registered managers, so a
    /// hand-built `encodedTrack` naming a source the operator switched off was
    /// still handed to `StreamOpener::open`, which dispatches on that name alone.
    /// With `local` that reads any path on the filesystem — on a node whose config
    /// says `sources.local: false`.
    #[test]
    fn an_unregistered_source_is_refused_however_well_formed() {
        let (loader, _) = loader(ok_track);
        let mut track = one_track();
        track.info.source_name = "local".into();
        track.info.identifier = "/etc/shadow".into();
        let encoded = encode(track).unwrap().encoded;

        let error = loader.decode(&encoded).unwrap_err();
        assert_eq!(error.severity, Severity::Common);
        assert!(error.cause.contains("local"), "cause was {:?}", error.cause);
    }

    /// The bug: `LeaderGuard::drop` used to remove `in_flight[identifier]` by key
    /// alone. If a new leader had already registered its own sender under the
    /// same identifier — the timing a cancelled leader's guard can race — that
    /// unconditional removal would delete the *new* leader's live sender before
    /// it ever sends, forcing its followers to retry as leaders of their own
    /// instead of getting the result that was already on its way.
    #[test]
    fn a_stale_leader_guard_does_not_remove_a_different_leaders_entry() {
        let (loader, _) = loader(ok_track);
        let identifier = "https://example.invalid/a.mp3";

        let (sender_a, _) = broadcast::channel::<LoadResult>(1);
        let (sender_b, _) = broadcast::channel::<LoadResult>(1);

        lock(&loader.in_flight).insert(identifier.to_owned(), sender_a.clone());
        let stale_guard = LeaderGuard {
            loader: &loader,
            identifier,
            sender: sender_a,
        };

        // A new leader takes over the same key before the stale guard drops —
        // exactly the situation remove_if_still_ours exists to detect.
        lock(&loader.in_flight).insert(identifier.to_owned(), sender_b.clone());

        drop(stale_guard);

        assert!(
            lock(&loader.in_flight)
                .get(identifier)
                .is_some_and(|current| current.same_channel(&sender_b)),
            "a stale leader's guard must not remove a different leader's entry"
        );
    }

    /// A manager whose loader thread blocks until released — lets a test cancel
    /// the *leader's* awaiting task while the underlying OS thread (which nothing
    /// can cancel) is still running, the way a client-side request timeout would.
    struct Blocking {
        gate: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl SourceManager for Blocking {
        fn name(&self) -> &'static str {
            "http"
        }

        fn matches(&self, identifier: &str) -> bool {
            identifier.starts_with("https://")
        }

        fn load(&self, _identifier: &str) -> Result<SourceLoad, SourceError> {
            let (mutex, condvar) = &*self.gate;
            let mut released = mutex.lock().unwrap();
            while !*released {
                released = condvar.wait(released).unwrap();
            }
            ok_track()
        }
    }

    #[tokio::test]
    async fn a_cancelled_leader_frees_its_identifier_for_the_next_caller() {
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let loader = Arc::new(Loader::new(vec![Arc::new(Blocking { gate: Arc::clone(&gate) })]));
        let identifier = "https://example.invalid/a.mp3";

        let leader = {
            let loader = Arc::clone(&loader);
            let identifier = identifier.to_owned();
            tokio::spawn(async move { loader.load(&identifier).await })
        };

        // The leader registers itself synchronously before its first await, so
        // this settles as soon as the spawned task gets to run at all.
        while lock(&loader.in_flight).is_empty() {
            tokio::task::yield_now().await;
        }

        leader.abort();
        let _ = leader.await;

        assert!(
            lock(&loader.in_flight).is_empty(),
            "a cancelled leader must clear its own in_flight entry, not just its own task"
        );

        // Release the still-running thread so it doesn't outlive the test.
        {
            let (mutex, condvar) = &*gate;
            *mutex.lock().unwrap() = true;
            condvar.notify_all();
        }

        let result = tokio::time::timeout(Duration::from_secs(1), loader.load(identifier))
            .await
            .expect("a fresh load for the identifier must not hang behind the cancelled leader");
        assert!(matches!(result, LoadResult::Track(_)));
    }

    /// A manager like [`Blocking`], but counting invocations too — so a follower
    /// that wrongly falls through to its own independent load (defeating
    /// single-flight) is distinguishable from one that correctly waits for a new
    /// leader.
    struct CountingBlocking {
        gate: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        loads: Arc<AtomicUsize>,
    }

    impl SourceManager for CountingBlocking {
        fn name(&self) -> &'static str {
            "http"
        }

        fn matches(&self, identifier: &str) -> bool {
            identifier.starts_with("https://")
        }

        fn load(&self, _identifier: &str) -> Result<SourceLoad, SourceError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            let (mutex, condvar) = &*self.gate;
            let mut released = mutex.lock().unwrap();
            while !*released {
                released = condvar.wait(released).unwrap();
            }
            ok_track()
        }
    }

    /// The scenario `a_cancelled_leader_frees_its_identifier_for_the_next_caller`
    /// doesn't cover: followers already *subscribed* to a leader that then gets
    /// cancelled. They must not each fall through to their own independent load —
    /// exactly one of them should become the new leader, and the result they all
    /// receive must still be the one that gets cached.
    #[tokio::test]
    async fn followers_of_a_cancelled_leader_share_exactly_one_new_load() {
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let loads = Arc::new(AtomicUsize::new(0));
        let manager = CountingBlocking {
            gate: Arc::clone(&gate),
            loads: Arc::clone(&loads),
        };
        let loader = Arc::new(Loader::new(vec![Arc::new(manager)]));
        let identifier = "https://example.invalid/a.mp3";

        let leader = {
            let loader = Arc::clone(&loader);
            let identifier = identifier.to_owned();
            tokio::spawn(async move { loader.load(&identifier).await })
        };

        while lock(&loader.in_flight).is_empty() {
            tokio::task::yield_now().await;
        }

        let mut followers = Vec::new();
        for _ in 0..2 {
            let loader = Arc::clone(&loader);
            let identifier = identifier.to_owned();
            followers.push(tokio::spawn(async move { loader.load(&identifier).await }));
        }

        // Wait until both followers have subscribed to the leader's broadcast,
        // so the abort below actually exercises the "already waiting" path.
        loop {
            let subscribed = lock(&loader.in_flight)
                .get(identifier)
                .map(|sender| sender.receiver_count())
                .unwrap_or(0);
            if subscribed >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }

        leader.abort();
        let _ = leader.await;

        // Release every gated thread (the orphaned leader's and whichever
        // follower becomes the new leader's) so nothing outlives the test.
        {
            let (mutex, condvar) = &*gate;
            *mutex.lock().unwrap() = true;
            condvar.notify_all();
        }

        let (a, b) = tokio::join!(followers.remove(0), followers.remove(0));
        assert!(matches!(a.unwrap(), LoadResult::Track(_)));
        assert!(matches!(b.unwrap(), LoadResult::Track(_)));

        // One load for the aborted leader (its OS thread was already running and
        // can't be cancelled) plus exactly one for the new leader the followers
        // elected — never three.
        //
        // The orphaned leader's fetch_add races this assertion: spawn returning
        // only means the OS thread exists, not that the kernel has scheduled it,
        // and nothing here waits on it (deliberately orphaned, no join handle).
        // Poll instead of reading the counter once, since CI contention can
        // outlast a bare check.
        let deadline = Instant::now() + Duration::from_secs(2);
        while loads.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            loads.load(Ordering::SeqCst),
            2,
            "both followers must not each fall through to an independent load"
        );

        // And the new leader's result must have been cached, not discarded.
        let cached = tokio::time::timeout(Duration::from_secs(1), loader.load(identifier))
            .await
            .expect("a cached load must not hang");
        assert!(matches!(cached, LoadResult::Track(_)));
        assert_eq!(loads.load(Ordering::SeqCst), 2, "the third call must be served from cache");
    }

    /// A manager like [`Blocking`], but signaling when its `load` has actually
    /// started — so a test can wait for the OS thread to be running (and the
    /// permit to be taken) before acting, instead of racing the leader's own
    /// task scheduling.
    struct BlockingSignaling {
        started: Arc<std::sync::atomic::AtomicBool>,
        gate: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl SourceManager for BlockingSignaling {
        fn name(&self) -> &'static str {
            "http"
        }

        fn matches(&self, identifier: &str) -> bool {
            identifier.starts_with("https://")
        }

        fn load(&self, _identifier: &str) -> Result<SourceLoad, SourceError> {
            self.started.store(true, Ordering::SeqCst);
            let (mutex, condvar) = &*self.gate;
            let mut released = mutex.lock().unwrap();
            while !*released {
                released = condvar.wait(released).unwrap();
            }
            ok_track()
        }
    }

    /// The bug: the semaphore permit used to live in `load_uncached`'s own async
    /// stack frame, so cancelling the caller (dropping its future) freed the
    /// permit immediately even though the OS thread doing the real load — which
    /// nothing can cancel — kept running. A client that cancels and retries in a
    /// loop could then accumulate unbounded background loads while the semaphore
    /// reported slots free the whole time.
    #[tokio::test]
    async fn a_cancelled_leaders_permit_is_held_until_its_thread_actually_finishes() {
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let loader = Arc::new(Loader::new(vec![Arc::new(BlockingSignaling {
            started: Arc::clone(&started),
            gate: Arc::clone(&gate),
        })]));
        let identifier = "https://example.invalid/a.mp3";

        let leader = {
            let loader = Arc::clone(&loader);
            let identifier = identifier.to_owned();
            tokio::spawn(async move { loader.load(&identifier).await })
        };

        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }

        leader.abort();
        let _ = leader.await;

        assert_eq!(
            loader.permits.available_permits(),
            MAX_CONCURRENT_LOADS - 1,
            "a cancelled leader's permit must stay held until its thread actually finishes"
        );

        // Release the still-running thread so it doesn't outlive the test.
        {
            let (mutex, condvar) = &*gate;
            *mutex.lock().unwrap() = true;
            condvar.notify_all();
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            while loader.permits.available_permits() < MAX_CONCURRENT_LOADS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the permit must be released once the thread actually finishes");
    }
}
