//! Multi-edition completeness — maintain N editions per work (#494 epic, #531).
//!
//! The completeness reconcile (#496) answers "which *works* am I missing." This
//! answers the orthogonal question "for the works I DO have, which *editions* am
//! I missing" — so orca can keep BOTH a 4K and a 1080p movie, or an epub AND an
//! m4b of a book, side by side. Compose the two: reconcile finds absent works,
//! this finds absent editions of present works.
//!
//! [`EditionPolicy`] is distinct from [`crate::QualityPolicy`]: QualityPolicy
//! picks ONE best release + an upgrade path; EditionPolicy declares a SET of
//! editions to hold simultaneously. Each gap can then be turned into an
//! edition-pinned want-request (a QualityPolicy whose sole preferred token is
//! the missing edition) so grabbing 4K never clobbers the 1080p already held.
//!
//! Pure + generic across media types + backends. Editions compare
//! case-insensitively (`2160P` == `2160p`).

use crate::MediaIdentity;
use crate::identity;
use crate::seam::LibraryEntry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The set of editions to maintain per work. Order is preserved in gap reports
/// (best-first is a sensible convention but not required by the logic).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditionPolicy {
    /// Editions to hold simultaneously (`["2160p", "1080p"]`, `["epub", "m4b"]`).
    pub desired: Vec<String>,
}

/// For one expected work: which desired editions are not yet held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditionGap {
    pub identity: MediaIdentity,
    /// Missing desired editions, in policy order, original casing preserved.
    pub missing: Vec<String>,
    /// True when the work is entirely absent (every desired edition missing),
    /// vs a partially-held work missing only some editions. Lets the caller
    /// route a wholly-absent work through the #496 work-level path if it prefers.
    pub work_absent: bool,
}

fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Match keys for an identity, mirroring the reconcile matcher: canonical
/// external ids, series (name, sequence), and (title, year) fallback.
struct Keys {
    ext: Vec<(String, String)>,
    series: Option<(String, String)>,
    title: (String, Option<u16>),
}

fn keys(id: &MediaIdentity) -> Keys {
    let ext = id
        .external_ids
        .iter()
        .map(|e| {
            let c = identity::canonicalize(e);
            (c.source, c.id)
        })
        .collect();
    let series = id
        .series
        .as_ref()
        .and_then(|s| s.sequence.as_ref().map(|seq| (norm(&s.name), norm(seq))));
    Keys {
        ext,
        series,
        title: (norm(&id.title), id.year),
    }
}

/// Index owned entries by every match key → the editions held (lowercased).
struct OwnedEditions {
    by_ext: HashMap<(String, String), Vec<String>>,
    by_series: HashMap<(String, String), Vec<String>>,
    by_title: HashMap<(String, Option<u16>), Vec<String>>,
}

impl OwnedEditions {
    fn build(owned: &[LibraryEntry]) -> Self {
        let mut by_ext: HashMap<(String, String), Vec<String>> = HashMap::new();
        let mut by_series: HashMap<(String, String), Vec<String>> = HashMap::new();
        let mut by_title: HashMap<(String, Option<u16>), Vec<String>> = HashMap::new();
        for e in owned {
            let eds: Vec<String> = e.editions.iter().map(|x| norm(x)).collect();
            let k = keys(&e.identity);
            for key in k.ext {
                by_ext.entry(key).or_default().extend(eds.clone());
            }
            if let Some(s) = k.series {
                by_series.entry(s).or_default().extend(eds.clone());
            }
            by_title.entry(k.title).or_default().extend(eds);
        }
        OwnedEditions {
            by_ext,
            by_series,
            by_title,
        }
    }

    /// The editions held for an expected identity (union across every key it
    /// matches), or `None` if the work is not present at all.
    fn held(&self, expected: &MediaIdentity) -> Option<Vec<String>> {
        let k = keys(expected);
        let mut found = false;
        let mut eds: Vec<String> = Vec::new();
        for key in &k.ext {
            if let Some(v) = self.by_ext.get(key) {
                found = true;
                eds.extend(v.iter().cloned());
            }
        }
        if let Some(s) = &k.series
            && let Some(v) = self.by_series.get(s)
        {
            found = true;
            eds.extend(v.iter().cloned());
        }
        if let Some(v) = self.by_title.get(&k.title) {
            found = true;
            eds.extend(v.iter().cloned());
        }
        if found { Some(eds) } else { None }
    }
}

/// For each expected work, compute which desired editions are missing. A work
/// with no missing editions produces no gap. Absent works report every desired
/// edition missing with `work_absent = true`.
pub fn compute_edition_gaps(
    expected: &[MediaIdentity],
    owned: &[LibraryEntry],
    policy: &EditionPolicy,
) -> Vec<EditionGap> {
    if policy.desired.is_empty() {
        return Vec::new();
    }
    let idx = OwnedEditions::build(owned);
    let mut gaps = Vec::new();
    for id in expected {
        let held = idx.held(id);
        let work_absent = held.is_none();
        let held = held.unwrap_or_default();
        let missing: Vec<String> = policy
            .desired
            .iter()
            .filter(|d| !held.contains(&norm(d)))
            .cloned()
            .collect();
        if !missing.is_empty() {
            gaps.push(EditionGap {
                identity: id.clone(),
                missing,
                work_absent,
            });
        }
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExternalId, SeriesRef};

    fn tmdb(title: &str, id: &str, editions: &[&str]) -> LibraryEntry {
        LibraryEntry {
            identity: MediaIdentity {
                title: title.into(),
                year: None,
                external_ids: vec![ExternalId {
                    source: "tmdb".into(),
                    id: id.into(),
                }],
                series: None,
            },
            id: None,
            editions: editions.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn want(title: &str, id: &str) -> MediaIdentity {
        MediaIdentity {
            title: title.into(),
            year: None,
            external_ids: vec![ExternalId {
                source: "tmdb".into(),
                id: id.into(),
            }],
            series: None,
        }
    }

    fn policy(eds: &[&str]) -> EditionPolicy {
        EditionPolicy {
            desired: eds.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn owns_1080p_wants_both_reports_4k_gap() {
        let expected = vec![want("Blade Runner 2049", "335984")];
        let owned = vec![tmdb("Blade Runner 2049", "335984", &["1080p"])];
        let gaps = compute_edition_gaps(&expected, &owned, &policy(&["2160p", "1080p"]));
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].missing, vec!["2160p".to_string()]);
        assert!(!gaps[0].work_absent);
    }

    #[test]
    fn both_editions_present_is_no_gap_case_insensitive() {
        let expected = vec![want("Dune", "438631")];
        let owned = vec![tmdb("Dune", "438631", &["2160P", "1080p"])];
        let gaps = compute_edition_gaps(&expected, &owned, &policy(&["2160p", "1080p"]));
        assert!(gaps.is_empty());
    }

    #[test]
    fn absent_work_reports_all_editions_and_work_absent() {
        let expected = vec![want("Sicario", "273481")];
        let gaps = compute_edition_gaps(&expected, &[], &policy(&["2160p", "1080p"]));
        assert_eq!(gaps.len(), 1);
        assert_eq!(
            gaps[0].missing,
            vec!["2160p".to_string(), "1080p".to_string()]
        );
        assert!(gaps[0].work_absent);
    }

    #[test]
    fn book_epub_plus_m4b_matched_by_series_sequence() {
        // No external id; matched by series+sequence, one format held.
        let expected = vec![MediaIdentity {
            title: "Words of Radiance".into(),
            year: None,
            external_ids: vec![],
            series: Some(SeriesRef {
                name: "Stormlight".into(),
                sequence: Some("2".into()),
            }),
        }];
        let owned = vec![LibraryEntry {
            identity: MediaIdentity {
                title: "words of radiance".into(),
                year: None,
                external_ids: vec![],
                series: Some(SeriesRef {
                    name: "stormlight".into(),
                    sequence: Some("2".into()),
                }),
            },
            id: None,
            editions: vec!["epub".into()],
        }];
        let gaps = compute_edition_gaps(&expected, &owned, &policy(&["epub", "m4b"]));
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].missing, vec!["m4b".to_string()]);
        assert!(!gaps[0].work_absent);
    }

    #[test]
    fn empty_policy_yields_no_gaps() {
        let expected = vec![want("X", "1")];
        assert!(compute_edition_gaps(&expected, &[], &EditionPolicy::default()).is_empty());
    }
}
