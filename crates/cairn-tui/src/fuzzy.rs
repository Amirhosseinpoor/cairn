//! Fuzzy matching for the popups (history search, `@file`, `/command`).

/// How well `query` matches `candidate` as a subsequence, or `None`.
///
/// Higher is better: consecutive runs, matches at word starts and early
/// matches score more. Case-insensitive; an empty query matches everything
/// equally.
#[must_use]
pub fn score(query: &str, candidate: &str) -> Option<i64> {
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if q.is_empty() {
        return Some(0);
    }
    let c: Vec<char> = candidate.chars().collect();
    let lower: Vec<char> = c.iter().flat_map(|ch| ch.to_lowercase()).collect();
    // Case folding can change lengths for exotic characters; fall back to a
    // plain subsequence test in that case.
    if lower.len() != c.len() {
        return subsequence(&q, &lower).then_some(1);
    }
    let mut total = 0_i64;
    let mut qi = 0;
    let mut last_match: Option<usize> = None;
    for (i, ch) in lower.iter().enumerate() {
        if qi < q.len() && *ch == q[qi] {
            let mut gain = 10;
            if last_match.is_some_and(|l| l + 1 == i) {
                gain += 15;
            }
            let word_start = i == 0
                || !c[i - 1].is_alphanumeric()
                || (c[i - 1].is_lowercase() && c[i].is_uppercase());
            if word_start {
                gain += 12;
            }
            // Earlier is better, but only a little.
            gain += i64::try_from(10_usize.saturating_sub(i.min(10))).unwrap_or(0);
            total += gain;
            last_match = Some(i);
            qi += 1;
        }
    }
    if qi < q.len() {
        return None;
    }
    Some(total)
}

fn subsequence(q: &[char], hay: &[char]) -> bool {
    let mut qi = 0;
    for ch in hay {
        if qi < q.len() && *ch == q[qi] {
            qi += 1;
        }
    }
    qi == q.len()
}

/// [`rank`], but between equal scores the shorter candidate wins: what file
/// and command popups want, where `lib.rs` should beat `library_extra.rs`.
#[must_use]
pub fn rank_short_first<'a>(
    query: &str,
    candidates: impl IntoIterator<Item = &'a str>,
    limit: usize,
) -> Vec<&'a str> {
    let mut scored: Vec<(i64, usize, usize, &str)> = candidates
        .into_iter()
        .enumerate()
        .filter_map(|(i, c)| score(query, c).map(|s| (s, c.chars().count(), i, c)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, _, _, c)| c)
        .collect()
}

/// The `limit` best matches of `query` among `candidates`, best first; ties
/// keep the earlier candidate first.
#[must_use]
pub fn rank<'a>(
    query: &str,
    candidates: impl IntoIterator<Item = &'a str>,
    limit: usize,
) -> Vec<&'a str> {
    let mut scored: Vec<(i64, usize, &str)> = candidates
        .into_iter()
        .enumerate()
        .filter_map(|(i, c)| score(query, c).map(|s| (s, i, c)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(limit).map(|(_, _, c)| c).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subsequence_matches_and_anything_else_does_not() {
        assert!(score("spr", "src/parser.rs").is_some());
        assert!(score("xyz", "src/parser.rs").is_none());
        assert!(score("", "anything").is_some());
        assert!(score("SRC", "src/lib.rs").is_some(), "case-insensitive");
    }

    #[test]
    fn consecutive_and_word_start_matches_beat_scattered_ones() {
        let best = rank("pars", ["a_p_a_r_s", "src/parser.rs", "paragraphs"], 3);
        assert_eq!(best[0], "src/parser.rs");
        let best = rank("sr", ["rustfmt.toml", "src/lib.rs", "docs/readme.md"], 3);
        assert_eq!(best[0], "src/lib.rs", "{best:?}");
    }

    #[test]
    fn shorter_names_win_ties_and_the_limit_applies() {
        let best = rank_short_first("lib", ["src/lib.rs", "lib.rs", "library_extra.rs"], 2);
        assert_eq!(best.len(), 2);
        assert_eq!(best[0], "lib.rs");
        // Plain `rank` leaves equal scores in input order.
        assert_eq!(rank("a", ["xa", "ya"], 2), ["xa", "ya"]);
        let none: Vec<&str> = rank("zzz", ["a", "b"], 5);
        assert!(none.is_empty());
    }

    #[test]
    fn equal_scores_keep_input_order() {
        let r = rank("a", ["xa", "ya", "za"], 3);
        assert_eq!(r, ["xa", "ya", "za"]);
    }
}
