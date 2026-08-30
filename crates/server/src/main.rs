use std::backtrace::Backtrace;
use std::error::Error;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lavalink_server::audio::source::{
    configured_proxy, BandcampSource, DeezerSource, GetyarnSource, HttpSource, LocalSource,
    SoundCloudSource, SourceManager, YouTubeSource, YtDlp,
};
use lavalink_server::audio::stream::StreamOpener;
use lavalink_server::config::Config;
use lavalink_server::loader::Loader;
use lavalink_server::{rest, ticker, AppState};
use tracing_subscriber::EnvFilter;

// Symphonia's probe WARNs while successfully skipping ordinary ID3 padding, so
// keep that one noisy target at ERROR while exposing this server's own DEBUG path.
const DEFAULT_LOG_FILTER: &str =
    "info,lavalink_server=debug,symphonia_core::formats::probe=error";

/// Note the hand-built runtime rather than #[tokio::main].
///
/// Source managers hold reqwest::blocking clients, and building one inside a tokio
/// context panics — reqwest detects the runtime and refuses. So everything blocking
/// is constructed first, and the runtime is entered afterwards.
fn main() -> ExitCode {
    init_logging();
    install_panic_hook();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log_error_chain(error.as_ref());
            ExitCode::FAILURE
        }
    }
}

fn init_logging() {
    let filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        Err(error) => {
            if std::env::var_os(EnvFilter::DEFAULT_ENV).is_some() {
                eprintln!(
                    "invalid {} ({error:?}); using {DEFAULT_LOG_FILTER}",
                    EnvFilter::DEFAULT_ENV
                );
            }
            EnvFilter::new(DEFAULT_LOG_FILTER)
        }
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .init();
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let message = panic_message(info.payload());
        let location = info.location();
        let backtrace = Backtrace::force_capture();
        tracing::error!(
            panic_message = %message,
            panic_file = location.map(std::panic::Location::file).unwrap_or("<unknown>"),
            panic_line = location.map(std::panic::Location::line).unwrap_or(0),
            panic_column = location.map(std::panic::Location::column).unwrap_or(0),
            backtrace = %backtrace,
            "thread panicked"
        );
    }));
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_owned())
}

fn log_error_chain(error: &(dyn Error + 'static)) {
    tracing::error!(error = ?error, error_display = %error, "server terminated");
    let mut source = error.source();
    let mut depth = 1;
    while let Some(error) = source {
        tracing::error!(depth, error = ?error, error_display = %error, "caused by");
        source = error.source();
        depth += 1;
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let started_at = Instant::now();

    let config_path = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("application.yml"), PathBuf::from);
    let config = Config::load(&config_path)?;

    // Built once and cloned into every blocking client this node constructs — see
    // configured_proxy's docs for why this exists.
    let proxy = configured_proxy(&config.lavalink.server.http_config)?;

    // yt-dlp is a runtime-optional dependency: detected here, and if it is absent
    // every source needing it is not registered and not advertised. The node runs
    // regardless. It is detected once and shared, because several sources use it —
    // Deezer included, despite loading through its own API, since its playback
    // still substitutes a YouTube match resolved through this same handle.
    let sources = &config.lavalink.server.sources;
    // The original's unit is pages of 100.
    let playlist_track_limit =
        config.lavalink.server.youtube_playlist_load_limit as usize * 100;
    let ytdlp = if sources.youtube || sources.soundcloud || sources.bandcamp || sources.deezer {
        match YtDlp::detect(
            "yt-dlp",
            config.lavalink.server.http_config.ytdlp_proxy_arg(),
            playlist_track_limit,
        ) {
            Some(backend) => {
                tracing::info!(version = %backend.version, "found yt-dlp");
                Some(Arc::new(backend))
            }
            None => {
                tracing::warn!(
                    "a source needing yt-dlp is enabled in the config but yt-dlp was not \
                     found; disabling those sources"
                );
                None
            }
        }
    } else {
        None
    };

    let loader = Loader::new(source_managers(&config, ytdlp.clone(), proxy.clone()));
    let timeouts = &config.lavalink.server.timeouts;
    let opener = StreamOpener::new(
        ytdlp,
        proxy,
        Duration::from_millis(timeouts.connect_timeout_ms),
        Duration::from_millis(timeouts.socket_timeout_ms),
    )?;
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let state = AppState::new(config, loader, opener, started_at, shutdown_rx);

    tracing::info!(
        sources = ?state.info.source_managers,
        filters = ?state.info.filters,
        "starting"
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve(state, shutdown_tx))
}

async fn serve(
    state: AppState,
    shutdown_tx: tokio::sync::watch::Sender<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    ticker::spawn(state.clone());

    let address = SocketAddr::new(state.config.server.address, state.config.server.port);
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(%address, "listening");

    axum::serve(listener, rest::router(state))
        .with_graceful_shutdown(shutdown_signal(shutdown_tx))
        .await?;

    Ok(())
}

/// Builds the source list, which is also exactly what /v4/info advertises.
///
/// Order matters: the first manager that claims an identifier gets it. yt-dlp goes
/// first so a YouTube URL is not swallowed by the generic HTTP source, which would
/// happily fetch the watch page and fail to find audio in it.
fn source_managers(
    config: &Config,
    ytdlp: Option<Arc<YtDlp>>,
    proxy: Option<reqwest::Proxy>,
) -> Vec<Arc<dyn SourceManager>> {
    let mut managers: Vec<Arc<dyn SourceManager>> = Vec::new();
    let sources = &config.lavalink.server.sources;

    if let Some(ytdlp) = ytdlp {
        if sources.youtube {
            managers.push(Arc::new(YouTubeSource::new(
                Arc::clone(&ytdlp),
                config.lavalink.server.youtube_search_enabled,
            )));
        }
        if sources.soundcloud {
            managers.push(Arc::new(SoundCloudSource::new(
                Arc::clone(&ytdlp),
                config.lavalink.server.soundcloud_search_enabled,
            )));
        }
        if sources.bandcamp {
            managers.push(Arc::new(BandcampSource::new(ytdlp)));
        }
        if sources.deezer {
            match DeezerSource::new(proxy.clone()) {
                Ok(source) => managers.push(Arc::new(source)),
                Err(error) => {
                    tracing::error!(
                        error_debug = ?error,
                        error_display = %error,
                        "could not start the deezer source; disabling it"
                    )
                }
            }
        }
    }
    if config.lavalink.server.sources.local {
        managers.push(Arc::new(LocalSource::new()));
    }
    if config.lavalink.server.sources.getyarn {
        // Must precede "http" below: getyarn.io URLs are https(s) too, and the
        // generic http source would otherwise claim them first and fetch the
        // page itself instead of the clip.
        match GetyarnSource::new(proxy.clone()) {
            Ok(source) => managers.push(Arc::new(source)),
            Err(error) => {
                tracing::error!(
                    error_debug = ?error,
                    error_display = %error,
                    "could not start the getyarn source; disabling it"
                )
            }
        }
    }
    if config.lavalink.server.sources.http {
        match HttpSource::new(proxy) {
            Ok(source) => managers.push(Arc::new(source)),
            // Advertising a source we could not build would be a lie in
            // /v4/info, so it is dropped rather than half-enabled.
            Err(error) => tracing::error!(
                error_debug = ?error,
                error_display = %error,
                "could not start the http source; disabling it"
            ),
        }
    }

    managers
}

/// Waits for SIGINT or SIGTERM, whichever an orchestrator sends — docker stop,
/// a Kubernetes pod eviction, and systemctl stop all send SIGTERM, not SIGINT,
/// so ctrl_c() alone never caught them and axum::serve's graceful shutdown
/// never ran on a normal restart.
///
/// Fires shutdown_tx before returning: axum::serve's own graceful shutdown
/// only stops accepting new connections and waits for existing ones to end on
/// their own, which an already-upgraded WebSocket never does by itself. Every
/// ws.rs connection watches this same channel to send a clean close frame
/// instead.
async fn shutdown_signal(shutdown_tx: tokio::sync::watch::Sender<()>) {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(
                error_debug = ?error,
                error_display = %error,
                "could not listen for Ctrl-C"
            );
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(
                    error_debug = ?error,
                    error_display = %error,
                    "could not install a SIGTERM handler"
                );
                std::future::pending::<()>().await;
            }
        }
    };
    // No SIGTERM equivalent on Windows; ctrl_c() above is the only signal there.
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutting down");
    if let Err(error) = shutdown_tx.send(()) {
        tracing::debug!(
            error_debug = ?error,
            error_display = %error,
            "no shutdown watchers remained"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_logging_exposes_server_debug_without_dependency_debug_noise() {
        assert!(DEFAULT_LOG_FILTER.contains("lavalink_server=debug"));
        assert!(DEFAULT_LOG_FILTER.starts_with("info,"));
    }

    #[test]
    fn panic_payloads_keep_the_original_message() {
        assert_eq!(panic_message(&"boom"), "boom");
        assert_eq!(panic_message(&"owned".to_owned()), "owned");
        assert_eq!(panic_message(&123_u32), "<non-string panic payload>");
    }

    #[test]
    fn specific_sources_precede_the_generic_http_source() {
        let mut config = Config::default();
        let sources = &mut config.lavalink.server.sources;
        sources.local = true;
        sources.http = true;
        sources.getyarn = true;

        let managers = source_managers(&config, None, None);
        let names: Vec<_> = managers.iter().map(|manager| manager.name()).collect();

        assert_eq!(names, ["local", "getyarn.io", "http"]);
    }
}
