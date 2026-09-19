//! Strict matching + junk-edition rejection (#496 epic, #499).
//!
//! orca-side gate on want-request candidates so a fuzzy backend matcher can
//! never grab the wrong item. This session's trap: LazyLibrarian's Google-Books
//! fuzzy matcher grabbed *The Waste Lands* for "Dark Tower VII", and surfaced
//! Cyrillic / shrink-wrap "editions" as candidates. The rule that stops it:
//!
//! - creator surname (when known) MUST appear in the candidate's creator field,
//! - the target title MUST appear as a normalized substring of the candidate,
//! - the candidate title MUST be all-ASCII (rejects foreign/junk editions),
//! - **never** a `rows[0]` fallback — if nothing matches, nothing is grabbed.
//!
//! Pure + generic: the seam [`crate::MediaIdentity`] carries no creator field
//! (movies/tv match on external id + year, not creator-surname), so the matcher
//! takes explicit title/creator strings the adapter supplies from whatever its
//! backend exposes. Creator checks are skipped when no creator is given.

use serde::{Deserialize, Serialize};

/// Which strict checks to enforce. Defaults to ALL on — the safe posture that
/// rejects junk; a caller can relax individual checks for a media type that
/// doesn't have the relevant field (e.g. a creator-less catalog).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchPolicy {
    /// Require the target creator's surname to appear in the candidate creator.
    pub require_creator_surname: bool,
    /// Require the normalized target title to be a substring of the candidate.
    pub require_title_substring: bool,
    /// Reject candidates whose title contains non-ASCII (foreign/junk editions).
    pub reject_non_ascii_title: bool,
}

impl Default for MatchPolicy {
    fn default() -> Self {
        MatchPolicy {
            require_creator_surname: true,
            require_title_substring: true,
            reject_non_ascii_title: true,
        }
    }
}

/// What we're trying to acquire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchTarget {
    pub title: String,
    /// Creator/author, when the media type has one (`"Stephen King"`).
    #[serde(default)]
    pub creator: Option<String>,
}

/// A candidate the backend returned (one search row / edition).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchCandidate {
    /// Opaque backend id, echoed back so the caller can act on the accepted row.
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub creator: Option<String>,
}

/// Why a candidate was rejected (for reporting alt-sourcing needs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// Target creator surname absent from the candidate's creator field.
    CreatorMismatch,
    /// Target title not found as a substring of the candidate title.
    TitleMismatch,
    /// Candidate title carries non-ASCII characters (foreign/junk edition).
    NonAsciiTitle,
}

/// A per-candidate verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub candidate_id: String,
    pub accepted: bool,
    /// First failing check when rejected (checks apply creator→title→ascii).
    #[serde(default)]
    pub reason: Option<RejectReason>,
}

/// The result of judging a candidate set against a target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchResult {
    /// Ids of candidates that passed every enforced check.
    pub accepted: Vec<String>,
    /// Per-candidate verdicts (accepted + rejected), input order preserved.
    pub verdicts: Vec<Verdict>,
    /// True when there were candidates but NONE matched — the "junk-only" signal
    /// the caller reports so the title can be sourced elsewhere (#499).
    pub junk_only: bool,
}

/// Normalize a string for comparison: lowercased, non-alphanumeric collapsed to
/// single spaces, trimmed. Makes "The Waste Lands" vs "waste-lands" comparable.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true; // trims leading
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// The surname = last whitespace-separated token of a creator name, normalized.
/// `"Brandon Sanderson"` → `"sanderson"`; `"King, Stephen"` → `"stephen"` (the
/// caller should pass display order, but a trailing token still beats nothing).
fn surname(creator: &str) -> Option<String> {
    normalize(creator)
        .split(' ')
        .next_back()
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
}

/// Judge one candidate against the target under the policy. Returns the first
/// failing check (creator, then title, then ascii) or acceptance.
pub fn judge(target: &MatchTarget, cand: &MatchCandidate, policy: &MatchPolicy) -> Verdict {
    let mk = |reason: Option<RejectReason>| Verdict {
        candidate_id: cand.id.clone(),
        accepted: reason.is_none(),
        reason,
    };

    // Creator surname: only when both a target creator and its surname exist.
    if policy.require_creator_surname
        && let Some(tc) = target.creator.as_deref()
        && let Some(sur) = surname(tc)
    {
        let cand_creator = cand.creator.as_deref().map(normalize).unwrap_or_default();
        let hit = cand_creator.split(' ').any(|tok| tok == sur);
        if !hit {
            return mk(Some(RejectReason::CreatorMismatch));
        }
    }

    if policy.require_title_substring {
        let t = normalize(&target.title);
        let c = normalize(&cand.title);
        if t.is_empty() || !c.contains(&t) {
            return mk(Some(RejectReason::TitleMismatch));
        }
    }

    // ASCII check on the RAW title (normalize drops non-alnum, hiding Cyrillic).
    if policy.reject_non_ascii_title && !cand.title.is_ascii() {
        return mk(Some(RejectReason::NonAsciiTitle));
    }

    mk(None)
}

/// Judge every candidate; collect the accepted ids and flag junk-only. There is
/// deliberately NO fallback to the first row — an empty `accepted` means grab
/// nothing (#499's "never a rows[0] fallback").
pub fn evaluate(
    target: &MatchTarget,
    candidates: &[MatchCandidate],
    policy: &MatchPolicy,
) -> MatchResult {
    let verdicts: Vec<Verdict> = candidates
        .iter()
        .map(|c| judge(target, c, policy))
        .collect();
    let accepted: Vec<String> = verdicts
        .iter()
        .filter(|v| v.accepted)
        .map(|v| v.candidate_id.clone())
        .collect();
    let junk_only = !candidates.is_empty() && accepted.is_empty();
    MatchResult {
        accepted,
        verdicts,
        junk_only,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, title: &str, creator: Option<&str>) -> MatchCandidate {
        MatchCandidate {
            id: id.into(),
            title: title.into(),
            creator: creator.map(|s| s.into()),
        }
    }

    #[test]
    fn dark_tower_does_not_match_waste_lands() {
        // The exact trap: fuzzy backend offered "The Waste Lands" for the target.
        let target = MatchTarget {
            title: "The Dark Tower VII: The Dark Tower".into(),
            creator: Some("Stephen King".into()),
        };
        let cands = vec![
            cand("1", "The Waste Lands", Some("Stephen King")),
            cand(
                "2",
                "The Dark Tower VII: The Dark Tower",
                Some("Stephen King"),
            ),
        ];
        let r = evaluate(&target, &cands, &MatchPolicy::default());
        assert_eq!(r.accepted, vec!["2".to_string()]);
        assert!(!r.junk_only);
        assert_eq!(r.verdicts[0].reason, Some(RejectReason::TitleMismatch));
    }

    #[test]
    fn rejects_cyrillic_and_wrong_author_editions() {
        let target = MatchTarget {
            title: "Dune".into(),
            creator: Some("Frank Herbert".into()),
        };
        let cands = vec![
            cand("cyr", "Дюна", Some("Frank Herbert")),
            cand("wrong", "Dune", Some("Kevin J. Anderson")),
            cand("ok", "Dune (Deluxe Edition)", Some("Frank Herbert")),
        ];
        let r = evaluate(&target, &cands, &MatchPolicy::default());
        assert_eq!(r.accepted, vec!["ok".to_string()]);
        // Cyrillic title fails the title-substring check first (normalize strips
        // it to empty), before the ascii check — either way it's rejected.
        assert!(r.verdicts[0].reason.is_some());
        assert_eq!(r.verdicts[1].reason, Some(RejectReason::CreatorMismatch));
    }

    #[test]
    fn junk_only_flag_when_nothing_matches_no_rows0_fallback() {
        let target = MatchTarget {
            title: "Nonexistent Title".into(),
            creator: Some("Nobody".into()),
        };
        let cands = vec![cand("a", "Something Else", Some("Someone Else"))];
        let r = evaluate(&target, &cands, &MatchPolicy::default());
        assert!(r.accepted.is_empty(), "must NOT fall back to rows[0]");
        assert!(r.junk_only);
    }

    #[test]
    fn no_candidates_is_not_junk_only() {
        let target = MatchTarget {
            title: "X".into(),
            creator: None,
        };
        let r = evaluate(&target, &[], &MatchPolicy::default());
        assert!(!r.junk_only);
        assert!(r.accepted.is_empty());
    }

    #[test]
    fn creator_check_skipped_when_target_has_none() {
        let target = MatchTarget {
            title: "Inception".into(),
            creator: None,
        };
        let cands = vec![cand("m", "Inception", None)];
        let r = evaluate(&target, &cands, &MatchPolicy::default());
        assert_eq!(r.accepted, vec!["m".to_string()]);
    }
}
