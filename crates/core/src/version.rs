//! Version comparison for the update checker (PLAN.md "an update checker
//! linking to signed GitHub releases").
//!
//! Pure and offline. The network fetch belongs to the caller; the part that
//! must be right is deciding whether a fetched version is genuinely newer,
//! because getting it wrong either nags a user who is up to date or hides a
//! release that fixes their bug.
//!
//! String comparison is not enough: `"0.10.0" < "0.9.0"` lexically, which
//! would silently stop offering updates after the ninth minor release.

use serde::{Deserialize, Serialize};

/// A semantic version, tolerant of a leading `v` and of trailing pre-release
/// or build metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// Parse `1.2.3`, `v1.2.3`, `1.2`, `1.2.3-beta.1`, `1.2.3+build`.
    /// Returns `None` for anything without at least a numeric major.
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let text = text.strip_prefix(['v', 'V']).unwrap_or(text);
        // Pre-release and build metadata do not affect ordering here: a
        // pre-release is not offered as an update at all (see `is_newer`).
        let core = text.split(['-', '+']).next().unwrap_or(text);
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        Some(Version {
            major,
            minor,
            patch,
        })
    }

    /// Whether this version carries a pre-release suffix.
    pub fn is_prerelease(text: &str) -> bool {
        let text = text.trim();
        let text = text.strip_prefix(['v', 'V']).unwrap_or(text);
        text.contains('-')
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// What the update checker concluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateVerdict {
    UpToDate,
    Available {
        version: String,
        url: String,
    },
    /// The release feed could not be understood. Reported, never guessed at.
    Unknown {
        reason: String,
    },
}

/// Decide whether `latest` is an update over `current`.
///
/// Pre-releases are never offered: a user running a stable build did not ask
/// to be moved onto a beta. A release with no usable version string is
/// `Unknown` rather than silently treated as "up to date", so a broken feed is
/// visible instead of quietly hiding every future release.
pub fn check(current: &str, latest_tag: &str, url: &str) -> UpdateVerdict {
    let Some(current_version) = Version::parse(current) else {
        return UpdateVerdict::Unknown {
            reason: format!("this build's version is unparseable: {current:?}"),
        };
    };
    let Some(latest) = Version::parse(latest_tag) else {
        return UpdateVerdict::Unknown {
            reason: format!("the latest release tag is unparseable: {latest_tag:?}"),
        };
    };
    if Version::is_prerelease(latest_tag) {
        return UpdateVerdict::UpToDate;
    }
    if latest > current_version {
        UpdateVerdict::Available {
            version: latest.to_string(),
            url: url.to_string(),
        }
    } else {
        UpdateVerdict::UpToDate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn available(current: &str, latest: &str) -> bool {
        matches!(
            check(current, latest, "https://example.invalid"),
            UpdateVerdict::Available { .. }
        )
    }

    #[test]
    fn versions_parse_in_the_shapes_releases_actually_use() {
        assert_eq!(Version::parse("1.2.3"), Version::parse("v1.2.3"));
        assert_eq!(
            Version::parse("0.1.0").unwrap(),
            Version {
                major: 0,
                minor: 1,
                patch: 0
            }
        );
        // Short and decorated forms still yield a comparable version.
        assert_eq!(Version::parse("2.1"), Version::parse("2.1.0"));
        assert_eq!(Version::parse("1.2.3-beta.1"), Version::parse("1.2.3"));
        assert_eq!(Version::parse("1.2.3+build7"), Version::parse("1.2.3"));
        assert!(Version::parse("not-a-version").is_none());
        assert!(Version::parse("").is_none());
    }

    /// The bug string comparison would introduce, and the reason this module
    /// exists: lexically, "0.10.0" sorts BEFORE "0.9.0".
    #[test]
    fn double_digit_components_compare_numerically() {
        assert!(available("0.9.0", "0.10.0"));
        assert!(!available("0.10.0", "0.9.0"));
        assert!(available("1.9.9", "1.10.0"));
        assert!(available("0.1.9", "0.1.10"));
    }

    #[test]
    fn an_update_is_offered_only_when_it_is_actually_newer() {
        assert!(available("0.1.0", "0.1.1"));
        assert!(available("0.1.0", "0.2.0"));
        assert!(available("0.1.0", "1.0.0"));
        assert!(!available("0.1.0", "0.1.0"));
        assert!(!available("0.2.0", "0.1.0"));
    }

    /// A stable user did not ask to be moved onto a beta.
    #[test]
    fn prereleases_are_never_offered() {
        assert!(!available("0.1.0", "0.2.0-beta.1"));
        assert!(!available("0.1.0", "v1.0.0-rc.2"));
        // ...but the stable release that follows is.
        assert!(available("0.1.0", "v1.0.0"));
    }

    /// A feed we cannot read must be visible, not silently "up to date" —
    /// otherwise a parsing change hides every future release.
    #[test]
    fn an_unreadable_feed_is_reported_rather_than_assumed_fine() {
        assert!(matches!(
            check("0.1.0", "latest", "u"),
            UpdateVerdict::Unknown { .. }
        ));
        assert!(matches!(
            check("nightly", "0.2.0", "u"),
            UpdateVerdict::Unknown { .. }
        ));
    }

    #[test]
    fn the_verdict_carries_where_to_get_it() {
        match check("0.1.0", "v0.2.0", "https://example.invalid/releases/v0.2.0") {
            UpdateVerdict::Available { version, url } => {
                assert_eq!(version, "0.2.0");
                assert!(url.ends_with("v0.2.0"));
            }
            other => panic!("expected an update, got {other:?}"),
        }
    }
}
