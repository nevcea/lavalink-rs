//! Lavalink v4 wire protocol.
//!
//! Pure serialization plus the lavaplayer `encodedTrack` codec — no server logic, no
//! async runtime, no I/O. Usable on its own by anything that speaks to or for a
//! Lavalink v4 node.
//!
//! The organising rule for this crate: **what a client can observe follows the
//! original exactly.** Where the original's shape looks like an accident —
//! `frameStats` omitted here but null there, `data: null` on an empty load result —
//! the accident is reproduced, and the reason is recorded next to the type rather
//! than argued with.

pub mod encoded_track;
pub mod filters;
pub mod http;
pub mod info;
pub mod java_io;
pub mod load_result;
pub mod message;
pub mod omissible;
pub mod player;
pub mod stats;

pub use encoded_track::{DecodedTrack, SourceTail};
pub use filters::Filters;
pub use http::{Error, Session, SessionUpdate};
pub use info::{Git, Info, Plugin, Version};
pub use load_result::{Exception, LoadResult, Playlist, PlaylistInfo, ResultStatus, Severity};
pub use message::{EmittedEvent, Message, TrackEndReason};
pub use omissible::Omissible;
pub use player::{
    EncodedTracks, JsonObject, Player, PlayerState, PlayerUpdate, PlayerUpdateTrack, Players, Track,
    TrackInfo, Tracks, VoiceState,
};
pub use stats::{Cpu, FrameStats, Memory, StatsData, StatsEvent};
