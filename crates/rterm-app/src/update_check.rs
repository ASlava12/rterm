//! GitHub-releases update probe.
//!
//! Hits `https://api.github.com/repos/<owner>/<repo>/releases/latest`,
//! parses `tag_name`, and compares it to `CARGO_PKG_VERSION`. The
//! comparison is intentionally lenient: tags get their leading `v`
//! stripped, and any non-numeric separator (`.`, `-`, `+`) splits
//! the value into a `Vec<u32>` that Rust's lexicographic ordering on
//! vectors compares correctly for SemVer-shaped values.
//!
//! Used in two places:
//!
//! - `--check-update` CLI flag: runs the probe synchronously,
//!   prints the result as a single line, exits.
//! - GUI startup: spawns a background thread that calls the same
//!   function and logs via `tracing::info` if a newer release is
//!   out. Failures (network down, GitHub 5xx, rate limit, ...) log
//!   at `debug` and are otherwise silent — no point pestering a
//!   user on a flight.

use std::time::Duration;

/// GitHub owner/repo for the release feed. Hard-coded rather than
/// pulled from `package.repository` so the probe still works when a
/// fork keeps the upstream Cargo.toml unchanged.
const REPO: &str = "ASlava12/rterm";

/// Found a newer release than the running binary.
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    /// Tag string verbatim from GitHub (e.g. `v0.0.3`).
    pub latest_tag: String,
    /// Browser URL of the release page — surfaced so the user can
    /// click straight through to changelog / downloads.
    pub html_url: String,
}

/// One-shot probe. `Ok(None)` = up to date / GitHub returned no
/// usable tag. `Ok(Some(_))` = newer release detected. `Err` is
/// reserved for caller-actionable problems (e.g. tests stubbing
/// network failures) — real-world callers can treat it the same as
/// `Ok(None)`.
pub fn check_latest() -> anyhow::Result<Option<UpdateInfo>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = ureq::get(&url)
        // GitHub's API requires a UA; rejects requests without one.
        // Including the current version makes our hits identifiable
        // in repo traffic and shows the maintainer who's still on
        // ancient builds.
        .set(
            "User-Agent",
            concat!("rterm/", env!("CARGO_PKG_VERSION")),
        )
        .set("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(10))
        .call()?;
    let json: serde_json::Value = resp.into_json()?;
    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let html_url = json
        .get("html_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if tag.is_empty() {
        return Ok(None);
    }
    let current = env!("CARGO_PKG_VERSION");
    if is_newer(&tag, current) {
        Ok(Some(UpdateInfo { latest_tag: tag, html_url }))
    } else {
        Ok(None)
    }
}

/// Fire-and-forget background check used by the GUI startup path.
/// Logs at `info` when an update is available so the user sees it
/// in stderr / journald without having to opt in; any failure
/// silently goes to `debug` (typical for offline / rate-limited
/// runs).
pub fn check_in_background() {
    std::thread::Builder::new()
        .name("rterm-update-check".into())
        .spawn(|| match check_latest() {
            Ok(Some(info)) => {
                tracing::info!(
                    latest = %info.latest_tag,
                    current = env!("CARGO_PKG_VERSION"),
                    url = %info.html_url,
                    "newer rterm release available",
                );
            }
            Ok(None) => {
                tracing::debug!("update check: up to date");
            }
            Err(e) => {
                tracing::debug!("update check failed (network?): {e}");
            }
        })
        .ok();
}

fn is_newer(latest: &str, current: &str) -> bool {
    use std::cmp::Ordering;
    let (lr, lp) = parse_version(latest);
    let (cr, cp) = parse_version(current);
    match lr.cmp(&cr) {
        Ordering::Greater => true,
        Ordering::Less => false,
        // Same release core (X.Y.Z). SemVer: a version WITH a
        // pre-release is OLDER than the same release without one, so a
        // prerelease tag must NOT read as "newer" than the current
        // stable build (the bug this guards against).
        Ordering::Equal => match (lp, cp) {
            (None, None) => false,       // identical stable
            (None, Some(_)) => true,     // stable release > pre-release
            (Some(_), None) => false,    // pre-release < stable release
            (Some(a), Some(b)) => a > b, // both pre-release: compare nums
        },
    }
}

/// Split a tag into `(release, pre_release)` numeric components.
/// `v0.0.3` → `([0, 0, 3], None)`; `v0.1.0-rc.2` → `([0, 1, 0], Some
/// ([2]))`; `+build.7` metadata is treated like a pre-release suffix.
/// Not a strict SemVer parser — strict ordering of textual pre-release
/// identifiers is out of scope — but it gets the release-vs-prerelease
/// precedence right, which a one-shot freshness check needs.
fn parse_version(s: &str) -> (Vec<u32>, Option<Vec<u32>>) {
    let s = s.trim_start_matches('v');
    let nums = |x: &str| -> Vec<u32> {
        x.split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect()
    };
    match s.find(['-', '+']) {
        Some(i) => (nums(&s[..i]), Some(nums(&s[i + 1..]))),
        None => (nums(s), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_splits_release_from_prerelease() {
        assert_eq!(parse_version("v0.0.3"), (vec![0, 0, 3], None));
        assert_eq!(parse_version("0.0.3"), (vec![0, 0, 3], None));
        assert_eq!(parse_version("v1.2.3-rc.4"), (vec![1, 2, 3], Some(vec![4])));
        assert_eq!(parse_version("v1.2.3+build.7"), (vec![1, 2, 3], Some(vec![7])));
        assert_eq!(parse_version(""), (Vec::<u32>::new(), None));
    }

    #[test]
    fn is_newer_orders_by_vector() {
        assert!(is_newer("v0.0.2", "0.0.1"));
        assert!(is_newer("v0.1.0", "0.0.99"));
        assert!(!is_newer("v0.0.1", "0.0.1"));
        assert!(!is_newer("v0.0.1", "0.0.2"));
    }

    #[test]
    fn prerelease_is_not_newer_than_its_release() {
        // The bug: a `-rc.N` tag marked "latest" must NOT read as newer
        // than the released stable — SemVer says pre-release < release.
        assert!(!is_newer("v0.0.13-rc.1", "0.0.13"));
        assert!(!is_newer("v0.0.13-rc.5", "0.0.13"));
        // A stable release IS newer than a running pre-release of it.
        assert!(is_newer("v0.0.13", "0.0.13-rc.1"));
        // Higher release core still wins regardless of pre-release.
        assert!(is_newer("v0.1.0-rc.1", "0.0.13"));
        // Two pre-releases of the same core order by their number.
        assert!(is_newer("v0.0.13-rc.2", "0.0.13-rc.1"));
        assert!(!is_newer("v0.0.13-rc.1", "0.0.13-rc.2"));
    }
}
