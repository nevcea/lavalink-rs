use serde::{Deserialize, Serialize};

/// `GET /v4/info`.
///
/// `sourceManagers` and `filters` list what this node *actually* runs, so that a
/// partial port advertises itself honestly. The `jvm` and `lavaplayer` fields have
/// no meaning here but stay in the shape because clients read them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub version: Version,
    pub build_time: i64,
    pub git: Git,
    pub jvm: String,
    pub lavaplayer: String,
    pub source_managers: Vec<String>,
    pub filters: Vec<String>,
    pub plugins: Vec<Plugin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Version {
    pub semver: String,
    pub major: i32,
    pub minor: i32,
    pub patch: i32,
    pub pre_release: Option<String>,
}

impl Version {
    /// Parses a semver string, falling back to `0.0.0` when it does not match —
    /// the same lenient behaviour as `Version.fromSemver` (`info.kt:59-63`).
    pub fn from_semver(semver: &str) -> Self {
        let fallback = || Self {
            semver: semver.to_owned(),
            major: 0,
            minor: 0,
            patch: 0,
            pre_release: None,
        };

        let (core, pre_release) = match semver.split_once('-') {
            Some((core, pre)) if !pre.is_empty() => (core, Some(pre.to_owned())),
            Some(_) => return fallback(),
            None => (semver, None),
        };

        let parts: Vec<&str> = core.split('.').collect();
        let [major, minor, patch] = parts.as_slice() else {
            return fallback();
        };
        let (Ok(major), Ok(minor), Ok(patch)) = (
            major.parse::<i32>(),
            minor.parse::<i32>(),
            patch.parse::<i32>(),
        ) else {
            return fallback();
        };

        Self {
            semver: semver.to_owned(),
            major,
            minor,
            patch,
            pre_release,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Git {
    pub branch: String,
    pub commit: String,
    /// Milliseconds since the epoch.
    pub commit_time: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plugin {
    pub name: String,
    pub version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_version() {
        let version = Version::from_semver("4.0.7");
        assert_eq!((version.major, version.minor, version.patch), (4, 0, 7));
        assert_eq!(version.pre_release, None);
    }

    #[test]
    fn parses_a_pre_release() {
        let version = Version::from_semver("4.1.0-beta.1");
        assert_eq!((version.major, version.minor, version.patch), (4, 1, 0));
        assert_eq!(version.pre_release.as_deref(), Some("beta.1"));
    }

    #[test]
    fn keeps_an_unparseable_version_as_zeros() {
        let version = Version::from_semver("not-a-version");
        assert_eq!((version.major, version.minor, version.patch), (0, 0, 0));
        assert_eq!(version.semver, "not-a-version");
    }
}
