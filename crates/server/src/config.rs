//! `application.yml` — the original's keys, kept so an existing deployment's config
//! file drops in unchanged.
//!
//! Keys belonging to features we do not ship (plugins, ratelimit, metrics, sentry,
//! logback) are simply not modelled; unknown keys are ignored rather than rejected,
//! because rejecting them would make a working Lavalink config fail to start here
//! for no gain.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: HttpServer,
    pub lavalink: Lavalink,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let config: Config = serde_yaml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.lavalink.server.password.is_empty() {
            return Err(ConfigError::Invalid(
                "lavalink.server.password must be set".into(),
            ));
        }
        if self.lavalink.server.player_update_interval == 0 {
            return Err(ConfigError::Invalid(
                "lavalink.server.playerUpdateInterval must be at least 1 second".into(),
            ));
        }
        Ok(())
    }

    /// Filters the client asked for that this node will refuse.
    ///
    /// Two sources feed this: filters switched off in the config, and filters we
    /// never implemented. The original only knows the first kind, but the wire
    /// behaviour — 400 listing the names — is identical, so clients see nothing new.
    pub fn disabled_filters(&self) -> Vec<String> {
        lavalink_protocol::filters::FILTER_ORDER
            .iter()
            .filter(|name| {
                let implemented = crate::audio::filter::IMPLEMENTED_FILTERS.contains(*name);
                let enabled = self
                    .lavalink
                    .server
                    .filters
                    .get(**name)
                    .copied()
                    .unwrap_or(true);
                !implemented || !enabled
            })
            .map(|name| (*name).to_owned())
            .collect()
    }

    pub fn enabled_filters(&self) -> Vec<String> {
        let disabled = self.disabled_filters();
        lavalink_protocol::filters::FILTER_ORDER
            .iter()
            .filter(|name| !disabled.iter().any(|d| d == *name))
            .map(|name| (*name).to_owned())
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HttpServer {
    pub port: u16,
    pub address: IpAddr,
}

impl Default for HttpServer {
    fn default() -> Self {
        Self {
            port: 2333,
            address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Lavalink {
    pub server: ServerConfig,
}

/// lavaplayer's `AudioConfiguration.ResamplingQuality`. `Low` is lavaplayer's own
/// default and is backed by the existing Catmull-Rom resampler (unchanged, zero
/// extra cost); `Medium`/`High` route through `rubato`'s windowed-sinc resampler —
/// see `audio/resample.rs`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ResamplingQuality {
    #[default]
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ServerConfig {
    pub password: String,
    pub sources: Sources,
    /// Absent names default to enabled, matching `InfoRestHandler.kt:36-38`.
    pub filters: BTreeMap<String, bool>,
    /// Milliseconds of decoded audio buffered per player.
    pub frame_buffer_duration_ms: u32,
    pub resampling_quality: ResamplingQuality,
    /// How long a player may produce no audio before `TrackStuckEvent`.
    pub track_stuck_threshold_ms: u64,
    /// Seconds between `playerUpdate` messages.
    pub player_update_interval: u64,
    pub http_config: HttpConfig,
    /// Gates whether `YouTubeSource` claims `ytsearch:` at all. `false` makes an
    /// otherwise-enabled YouTube source still resolve direct URLs, just not
    /// bare search terms — the same distinction the original's key draws.
    pub youtube_search_enabled: bool,
    /// Same idea as `youtube_search_enabled`, for `scsearch:`.
    pub soundcloud_search_enabled: bool,
    /// The original's unit is "pages of 100"; ours is a flat track count, so this
    /// is multiplied by 100 in `main.rs` before it reaches `YtDlp`.
    pub youtube_playlist_load_limit: u32,
    pub timeouts: Timeouts,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            password: String::new(),
            sources: Sources::default(),
            filters: BTreeMap::new(),
            frame_buffer_duration_ms: 5000,
            resampling_quality: ResamplingQuality::default(),
            track_stuck_threshold_ms: 10_000,
            player_update_interval: 5,
            http_config: HttpConfig::default(),
            youtube_search_enabled: true,
            soundcloud_search_enabled: true,
            youtube_playlist_load_limit: 6,
            timeouts: Timeouts::default(),
        }
    }
}

/// `lavalink.server.timeouts`. Only the two keys that map onto something this
/// node actually has are modelled — see `MAINTENANCE.md` for
/// `connectionRequestTimeoutMs`, which does not.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Timeouts {
    pub connect_timeout_ms: u64,
    pub socket_timeout_ms: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect_timeout_ms: 3000,
            socket_timeout_ms: 3000,
        }
    }
}

/// A forward proxy every outbound HTTP request this node makes — every source's
/// own client, and yt-dlp's own fetches — should route through.
///
/// `application.yml.example` warns that running the `http` source without one
/// configured can expose this node's IP address to whatever it fetches; this is
/// what backs that warning.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HttpConfig {
    pub proxy_host: String,
    pub proxy_port: u16,
    pub proxy_user: String,
    pub proxy_password: String,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            proxy_host: String::new(),
            proxy_port: 3128,
            proxy_user: String::new(),
            proxy_password: String::new(),
        }
    }
}

impl HttpConfig {
    /// `None` when no proxy host is configured, which is the default — a proxy is
    /// opt-in, not assumed.
    fn host_port(&self) -> Option<String> {
        if self.proxy_host.is_empty() {
            return None;
        }
        Some(format!("{}:{}", self.proxy_host, self.proxy_port))
    }

    /// For `reqwest::Proxy::all`, which takes credentials through its own
    /// `basic_auth` builder call rather than embedded in the URL.
    pub fn reqwest_proxy_url(&self) -> Option<String> {
        self.host_port().map(|host_port| format!("http://{host_port}"))
    }

    pub fn basic_auth(&self) -> Option<(&str, &str)> {
        (!self.proxy_user.is_empty()).then_some((self.proxy_user.as_str(), self.proxy_password.as_str()))
    }

    /// The single `[user:password@]host:port` form yt-dlp's own `--proxy` flag
    /// expects credentials embedded in.
    pub fn ytdlp_proxy_arg(&self) -> Option<String> {
        let host_port = self.host_port()?;
        Some(match self.basic_auth() {
            Some((user, password)) => format!("http://{user}:{password}@{host_port}"),
            None => format!("http://{host_port}"),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Sources {
    pub local: bool,
    pub http: bool,
    /// Additionally requires yt-dlp to be present at startup; the config switch
    /// alone does not turn it on.
    pub youtube: bool,
    /// Also served by yt-dlp, and subject to the same startup detection.
    pub soundcloud: bool,
    /// Also served by yt-dlp, and subject to the same startup detection.
    pub bandcamp: bool,
    /// Needs no key of its own, but playback substitutes a YouTube match — see
    /// `audio::source::deezer` — so it is subject to the same yt-dlp detection as
    /// the sources above despite not using yt-dlp to load.
    pub deezer: bool,
    /// Does not use yt-dlp — see `audio::source::getyarn`.
    pub getyarn: bool,
}

impl Default for Sources {
    fn default() -> Self {
        // Mirrors the shipped example: http on, everything else off.
        Self {
            local: false,
            http: true,
            youtube: false,
            soundcloud: false,
            bandcamp: false,
            deezer: false,
            getyarn: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("could not parse {path}: {source}")]
    Parse {
        path: String,
        source: serde_yaml::Error,
    },
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
server:
  port: 2333
  address: 0.0.0.0
lavalink:
  server:
    password: "youshallnotpass"
    sources:
      youtube: false
      http: true
      local: false
    filters:
      volume: true
      equalizer: true
      lowPass: false
    bufferDurationMs: 400
    frameBufferDurationMs: 5000
    trackStuckThresholdMs: 10000
    playerUpdateInterval: 5
"#;

    fn example() -> Config {
        serde_yaml::from_str(EXAMPLE).unwrap()
    }

    #[test]
    fn parses_the_original_key_layout() {
        let config = example();
        assert_eq!(config.server.port, 2333);
        assert_eq!(config.lavalink.server.password, "youshallnotpass");
        assert!(config.lavalink.server.sources.http);
        assert!(!config.lavalink.server.sources.local);
        assert_eq!(config.lavalink.server.frame_buffer_duration_ms, 5000);
    }

    #[test]
    fn ignores_keys_for_features_we_dropped() {
        let yaml = format!("{EXAMPLE}\nmetrics:\n  prometheus:\n    enabled: false\nsentry:\n  dsn: \"\"\n");
        assert!(serde_yaml::from_str::<Config>(&yaml).is_ok());
    }

    #[test]
    fn unimplemented_filters_are_disabled_regardless_of_config() {
        let config = example();
        let disabled = config.disabled_filters();
        // Configured off.
        assert!(disabled.contains(&"lowPass".to_owned()));
        // Implemented (see `audio::filter`) and enabled — every filter now is.
        assert!(!disabled.contains(&"timescale".to_owned()));
        assert!(!disabled.contains(&"volume".to_owned()));
        assert!(!disabled.contains(&"equalizer".to_owned()));
        assert!(!disabled.contains(&"karaoke".to_owned()));
    }

    #[test]
    fn advertised_filters_are_the_complement_of_disabled() {
        let config = example();
        assert_eq!(
            config.enabled_filters(),
            vec![
                "volume",
                "equalizer",
                "karaoke",
                "timescale",
                "tremolo",
                "vibrato",
                "distortion",
                "rotation",
                "channelMix",
            ]
        );
    }

    #[test]
    fn empty_password_is_rejected() {
        let config = Config::default();
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn no_proxy_host_means_no_proxy() {
        let http_config = HttpConfig::default();
        assert!(http_config.reqwest_proxy_url().is_none());
        assert!(http_config.ytdlp_proxy_arg().is_none());
    }

    #[test]
    fn a_configured_proxy_without_auth() {
        let http_config = HttpConfig {
            proxy_host: "localhost".into(),
            proxy_port: 3128,
            ..HttpConfig::default()
        };
        assert_eq!(
            http_config.reqwest_proxy_url().as_deref(),
            Some("http://localhost:3128")
        );
        assert!(http_config.basic_auth().is_none());
        assert_eq!(
            http_config.ytdlp_proxy_arg().as_deref(),
            Some("http://localhost:3128")
        );
    }

    #[test]
    fn a_configured_proxy_with_auth_embeds_credentials_for_ytdlp_only() {
        let http_config = HttpConfig {
            proxy_host: "localhost".into(),
            proxy_port: 3128,
            proxy_user: "alice".into(),
            proxy_password: "hunter2".into(),
        };
        // reqwest takes credentials through its own builder call, not the URL.
        assert_eq!(
            http_config.reqwest_proxy_url().as_deref(),
            Some("http://localhost:3128")
        );
        assert_eq!(http_config.basic_auth(), Some(("alice", "hunter2")));
        // yt-dlp's `--proxy` flag has no separate auth mechanism.
        assert_eq!(
            http_config.ytdlp_proxy_arg().as_deref(),
            Some("http://alice:hunter2@localhost:3128")
        );
    }

    #[test]
    fn http_config_parses_the_originals_key_names() {
        let yaml = r#"
lavalink:
  server:
    password: "x"
    httpConfig:
      proxyHost: "localhost"
      proxyPort: 3128
      proxyUser: "alice"
      proxyPassword: "hunter2"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let http_config = config.lavalink.server.http_config;
        assert_eq!(http_config.proxy_host, "localhost");
        assert_eq!(http_config.proxy_port, 3128);
        assert_eq!(http_config.proxy_user, "alice");
        assert_eq!(http_config.proxy_password, "hunter2");
    }
}
