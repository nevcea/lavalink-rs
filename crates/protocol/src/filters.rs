//! Filter DTOs.
//!
//! All ten original filters are modeled here, independently of which ones a
//! server actually runs: the wire shape has to parse in full so that a request
//! naming a filter the node does not run can be rejected with the original's
//! 400 + name list (PlayerRestHandler.kt:90-95) rather than a parse error.
//! What is really implemented is the server's own list, not this crate's — see
//! lavalink-server's audio::filter::IMPLEMENTED_FILTERS.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::omissible::Omissible;

/// Filter names in the original's application order (FilterChain.kt:79-91).
///
/// The relative order is part of how the audio sounds, so it is pinned here and
/// consumed by the DSP chain rather than being re-derived per call site.
pub const FILTER_ORDER: [&str; 10] = [
    "volume",
    "equalizer",
    "karaoke",
    "timescale",
    "tremolo",
    "vibrato",
    "distortion",
    "rotation",
    "channelMix",
    "lowPass",
];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Filters {
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub volume: Omissible<f32>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub equalizer: Omissible<Vec<Band>>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub karaoke: Omissible<Option<Karaoke>>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub timescale: Omissible<Option<Timescale>>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub tremolo: Omissible<Option<Tremolo>>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub vibrato: Omissible<Option<Vibrato>>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub distortion: Omissible<Option<Distortion>>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub rotation: Omissible<Option<Rotation>>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub channel_mix: Omissible<Option<ChannelMix>>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub low_pass: Omissible<Option<LowPass>>,
    /// Always empty for us — we ship no plugins. Kept so a client that sends it
    /// round-trips instead of erroring.
    ///
    /// Not upstream's shape: v4 carries plugin filters as arbitrary top-level
    /// keys inside filters, and this nests them under a pluginFilters key
    /// and omits it when empty. Unobservable, because a plugin filter name only
    /// exists if a plugin defines one and this node has none — an unknown
    /// top-level key is dropped either way. See MAINTENANCE.md's "Post-auth
    /// resource limits and source reach" for why it is not worth restructuring
    /// Filters for a path with no reachable caller.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugin_filters: BTreeMap<String, Value>,
}

impl Filters {
    /// Names present in this request that the server has disabled.
    ///
    /// Mirrors Filters.validate (filters.kt:21-57) including its ordering, which
    /// the original joins into the 400 message verbatim — hence FILTER_ORDER
    /// driving the walk rather than a second list that could drift from it.
    pub fn validate(&self, disabled: &[String]) -> Vec<String> {
        FILTER_ORDER
            .iter()
            .copied()
            .filter(|name| self.contains(name))
            .chain(self.plugin_filters.keys().map(String::as_str))
            .filter(|name| disabled.iter().any(|entry| entry == name))
            .map(str::to_owned)
            .collect()
    }

    /// Whether the named filter appears in this request. Names are the wire names,
    /// i.e. the entries of FILTER_ORDER.
    fn contains(&self, name: &str) -> bool {
        match name {
            "volume" => self.volume.is_present(),
            "equalizer" => self.equalizer.is_present(),
            "karaoke" => self.karaoke.is_present(),
            "timescale" => self.timescale.is_present(),
            "tremolo" => self.tremolo.is_present(),
            "vibrato" => self.vibrato.is_present(),
            "distortion" => self.distortion.is_present(),
            "rotation" => self.rotation.is_present(),
            "channelMix" => self.channel_mix.is_present(),
            "lowPass" => self.low_pass.is_present(),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Band {
    pub band: i32,
    #[serde(default = "one")]
    pub gain: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Karaoke {
    #[serde(default = "one")]
    pub level: f32,
    #[serde(default = "one")]
    pub mono_level: f32,
    #[serde(default = "karaoke_filter_band")]
    pub filter_band: f32,
    #[serde(default = "karaoke_filter_width")]
    pub filter_width: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Timescale {
    #[serde(default = "one_f64")]
    pub speed: f64,
    #[serde(default = "one_f64")]
    pub pitch: f64,
    #[serde(default = "one_f64")]
    pub rate: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tremolo {
    #[serde(default = "two")]
    pub frequency: f32,
    #[serde(default = "half")]
    pub depth: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Vibrato {
    #[serde(default = "two")]
    pub frequency: f32,
    #[serde(default = "half")]
    pub depth: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rotation {
    #[serde(default)]
    pub rotation_hz: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Distortion {
    #[serde(default)]
    pub sin_offset: f32,
    #[serde(default = "one")]
    pub sin_scale: f32,
    #[serde(default)]
    pub cos_offset: f32,
    #[serde(default = "one")]
    pub cos_scale: f32,
    #[serde(default)]
    pub tan_offset: f32,
    #[serde(default = "one")]
    pub tan_scale: f32,
    #[serde(default)]
    pub offset: f32,
    #[serde(default = "one")]
    pub scale: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelMix {
    #[serde(default = "one")]
    pub left_to_left: f32,
    #[serde(default)]
    pub left_to_right: f32,
    #[serde(default)]
    pub right_to_left: f32,
    #[serde(default = "one")]
    pub right_to_right: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LowPass {
    #[serde(default = "low_pass_smoothing")]
    pub smoothing: f32,
}

fn one() -> f32 {
    1.0
}
fn one_f64() -> f64 {
    1.0
}
fn two() -> f32 {
    2.0
}
fn half() -> f32 {
    0.5
}
fn karaoke_filter_band() -> f32 {
    220.0
}
fn karaoke_filter_width() -> f32 {
    100.0
}
fn low_pass_smoothing() -> f32 {
    20.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"{
      "volume": 1.0,
      "equalizer": [{ "band": 0, "gain": 0.2 }],
      "karaoke": { "level": 1.0, "monoLevel": 1.0, "filterBand": 220.0, "filterWidth": 100.0 },
      "timescale": { "speed": 1.0, "pitch": 1.0, "rate": 1.0 },
      "tremolo": { "frequency": 2.0, "depth": 0.5 },
      "vibrato": { "frequency": 2.0, "depth": 0.5 },
      "rotation": { "rotationHz": 0 },
      "distortion": {
        "sinOffset": 0.0, "sinScale": 1.0, "cosOffset": 0.0, "cosScale": 1.0,
        "tanOffset": 0.0, "tanScale": 1.0, "offset": 0.0, "scale": 1.0
      },
      "channelMix": { "leftToLeft": 1.0, "leftToRight": 0.0, "rightToLeft": 0.0, "rightToRight": 1.0 },
      "lowPass": { "smoothing": 20.0 }
    }"#;

    #[test]
    fn full_filters_parse() {
        let filters: Filters = serde_json::from_str(FULL).unwrap();
        assert_eq!(filters.volume, Omissible::Present(1.0));
        assert_eq!(
            filters.equalizer,
            Omissible::Present(vec![Band {
                band: 0,
                gain: 0.2
            }])
        );
        assert_eq!(
            filters.low_pass,
            Omissible::Present(Some(LowPass { smoothing: 20.0 }))
        );
        assert!(filters.plugin_filters.is_empty());
    }

    #[test]
    fn empty_filters_omit_everything() {
        let filters: Filters = serde_json::from_str("{}").unwrap();
        assert_eq!(filters, Filters::default());
        assert_eq!(serde_json::to_string(&filters).unwrap(), "{}");
    }

    #[test]
    fn validate_reports_disabled_filters_in_original_order() {
        let filters: Filters = serde_json::from_str(FULL).unwrap();
        let disabled = vec!["lowPass".to_owned(), "volume".to_owned()];
        assert_eq!(filters.validate(&disabled), vec!["volume", "lowPass"]);
    }

    #[test]
    fn validate_ignores_absent_filters() {
        let filters: Filters = serde_json::from_str(r#"{"volume":0.5}"#).unwrap();
        let disabled = vec!["timescale".to_owned()];
        assert!(filters.validate(&disabled).is_empty());
    }
}
