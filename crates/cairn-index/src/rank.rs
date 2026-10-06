//! Ranking files for a repository map (SPEC §5.2, "Ranking algorithm").
//!
//! Personalised `PageRank` over the reference graph says which files matter
//! around the work in progress; BM25 over names, paths, signatures and docs
//! says which files match the question. The two are normalised and blended.

use std::collections::{BTreeMap, HashMap};

pub const DAMPING: f64 = 0.85;
pub const ITERATIONS: usize = 20;
pub const EPSILON: f64 = 1e-6;

/// Weights of the personalisation vector (§5.2).
pub const CURRENT_FILE_WEIGHT: f64 = 0.60;
pub const TOUCHED_WEIGHT: f64 = 0.30;
pub const BACKGROUND_WEIGHT: f64 = 0.10;

/// BM25 parameters and field boosts (§5.2).
pub const K1: f64 = 1.2;
pub const B: f64 = 0.75;
pub const BOOST_NAME: f64 = 3.0;
pub const BOOST_PATH: f64 = 1.5;
pub const BOOST_SIGNATURE: f64 = 1.2;
pub const BOOST_DOC: f64 = 1.0;

/// A repository below this size ranks by BM25 alone: its graph is too sparse
/// for `PageRank` to say anything.
pub const SPARSE_FILES: usize = 50;

/// The blend (§5.2).
pub const PAGERANK_SHARE: f64 = 0.55;
pub const BM25_SHARE: f64 = 0.45;

/// Every number the ranking uses. The defaults are §5.2's; `repo_map.*` in
/// the configuration overrides them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    pub damping: f64,
    pub iterations: usize,
    pub current_file: f64,
    pub touched: f64,
    pub uniform: f64,
    pub pagerank_share: f64,
    pub bm25_share: f64,
    pub k1: f64,
    pub b: f64,
    pub name_boost: f64,
    pub path_boost: f64,
    pub signature_boost: f64,
    pub doc_boost: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            damping: DAMPING,
            iterations: ITERATIONS,
            current_file: CURRENT_FILE_WEIGHT,
            touched: TOUCHED_WEIGHT,
            uniform: BACKGROUND_WEIGHT,
            pagerank_share: PAGERANK_SHARE,
            bm25_share: BM25_SHARE,
            k1: K1,
            b: B,
            name_boost: BOOST_NAME,
            path_boost: BOOST_PATH,
            signature_boost: BOOST_SIGNATURE,
            doc_boost: BOOST_DOC,
        }
    }
}

/// A count as a float. Counts here are file and term numbers, nowhere near
/// the 2^52 where `f64` stops being exact.
#[allow(clippy::cast_precision_loss)]
const fn float(n: usize) -> f64 {
    n as f64
}

/// A directed, weighted edge between file indexes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub weight: f64,
}

/// The §5.2 personalisation vector for `n` files.
///
/// `current` and `touched` are indexes; the 10% background is spread evenly.
#[must_use]
pub fn personalization(n: usize, current: Option<usize>, touched: &[usize]) -> Vec<f64> {
    personalization_with(n, current, touched, &Params::default())
}

/// [`personalization`] with the weights in `params`.
#[must_use]
pub fn personalization_with(
    n: usize,
    current: Option<usize>,
    touched: &[usize],
    params: &Params,
) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }
    let mut p = vec![params.uniform / float(n); n];
    if let Some(c) = current.filter(|c| *c < n) {
        p[c] += params.current_file;
    }
    let touched: Vec<usize> = {
        let mut t: Vec<usize> = touched.iter().copied().filter(|i| *i < n).collect();
        t.sort_unstable();
        t.dedup();
        t
    };
    for i in &touched {
        p[*i] += params.touched / float(touched.len());
    }
    // Weights that were not used (no current file, no touched files) go to
    // the background, so the vector always sums to one.
    let sum: f64 = p.iter().sum();
    p.iter().map(|v| v / sum).collect()
}

/// Personalised `PageRank`. Mass on files with no outgoing edges is returned to
/// the personalisation vector, so the scores sum to one.
#[must_use]
pub fn pagerank(n: usize, edges: &[Edge], personalization: &[f64]) -> Vec<f64> {
    pagerank_with(n, edges, personalization, &Params::default())
}

/// [`pagerank`] with the damping and iteration count in `params`.
#[must_use]
pub fn pagerank_with(
    n: usize,
    edges: &[Edge],
    personalization: &[f64],
    params: &Params,
) -> Vec<f64> {
    let damping = params.damping;
    if n == 0 {
        return Vec::new();
    }
    let mut out_weight = vec![0.0_f64; n];
    for e in edges {
        if e.from < n && e.to < n && e.weight > 0.0 {
            out_weight[e.from] += e.weight;
        }
    }
    let mut rank = personalization.to_vec();
    for _ in 0..params.iterations {
        let mut next = vec![0.0_f64; n];
        let mut dangling = 0.0;
        for (i, w) in out_weight.iter().enumerate() {
            if *w == 0.0 {
                dangling += rank[i];
            }
        }
        for e in edges {
            if e.from < n && e.to < n && e.weight > 0.0 {
                next[e.to] += damping * rank[e.from] * e.weight / out_weight[e.from];
            }
        }
        for i in 0..n {
            next[i] +=
                damping * dangling * personalization[i] + (1.0 - damping) * personalization[i];
        }
        let delta: f64 = next.iter().zip(&rank).map(|(a, b)| (a - b).abs()).sum();
        rank = next;
        if delta < EPSILON {
            break;
        }
    }
    rank
}

/// Lower-case words of `text`, split on non-alphanumerics and on camelCase
/// and digit boundaries (`parseHTTPRequest` → `parse`, `http`, `request`).
#[must_use]
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut prev: Option<char> = None;
    let chars: Vec<char> = text.chars().collect();
    let flush = |word: &mut String, out: &mut Vec<String>| {
        if !word.is_empty() {
            out.push(word.to_lowercase());
            word.clear();
        }
    };
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            flush(&mut word, &mut out);
            prev = None;
            continue;
        }
        let next = chars.get(i + 1).copied();
        let boundary = prev.is_some_and(|p| {
            (p.is_lowercase() && c.is_uppercase())
                || (p.is_uppercase() && c.is_uppercase() && next.is_some_and(char::is_lowercase))
                || (p.is_alphabetic() && c.is_numeric())
                || (p.is_numeric() && c.is_alphabetic())
        });
        if boundary {
            flush(&mut word, &mut out);
        }
        word.push(c);
        prev = Some(c);
    }
    flush(&mut word, &mut out);
    out
}

/// One file's searchable text, by field.
#[derive(Debug, Clone, Default)]
pub struct Document {
    pub names: String,
    pub path: String,
    pub signatures: String,
    pub docs: String,
}

/// BM25 over [`Document`]s with field boosts (a weighted term frequency, as in
/// BM25F).
#[derive(Debug)]
pub struct Corpus {
    k1: f64,
    b: f64,
    /// Weighted term frequency per document.
    tf: Vec<HashMap<String, f64>>,
    len: Vec<f64>,
    avg_len: f64,
    df: HashMap<String, usize>,
}

impl Corpus {
    #[must_use]
    pub fn new(docs: &[Document]) -> Self {
        Self::with(docs, &Params::default())
    }

    /// [`Corpus::new`] with the boosts and BM25 constants in `params`.
    #[must_use]
    pub fn with(docs: &[Document], params: &Params) -> Self {
        let mut tf = Vec::with_capacity(docs.len());
        let mut len = Vec::with_capacity(docs.len());
        let mut df: HashMap<String, usize> = HashMap::new();
        for doc in docs {
            let mut weights: HashMap<String, f64> = HashMap::new();
            let mut total = 0.0;
            for (text, boost) in [
                (&doc.names, params.name_boost),
                (&doc.path, params.path_boost),
                (&doc.signatures, params.signature_boost),
                (&doc.docs, params.doc_boost),
            ] {
                for token in tokens(text) {
                    *weights.entry(token).or_default() += boost;
                    total += boost;
                }
            }
            for token in weights.keys() {
                *df.entry(token.clone()).or_default() += 1;
            }
            tf.push(weights);
            len.push(total);
        }
        let avg_len = if docs.is_empty() {
            0.0
        } else {
            len.iter().sum::<f64>() / float(docs.len())
        };
        Self {
            k1: params.k1,
            b: params.b,
            tf,
            len,
            avg_len,
            df,
        }
    }

    /// BM25 of every document for `query`.
    #[must_use]
    pub fn score(&self, query: &str) -> Vec<f64> {
        let n = float(self.tf.len());
        let mut terms = tokens(query);
        terms.sort();
        terms.dedup();
        self.tf
            .iter()
            .zip(&self.len)
            .map(|(weights, len)| {
                terms
                    .iter()
                    .map(|term| {
                        let Some(tf) = weights.get(term) else {
                            return 0.0;
                        };
                        let df = float(self.df.get(term).copied().unwrap_or(0));
                        let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                        let norm = 1.0 - self.b + self.b * len / self.avg_len.max(1e-9);
                        idf * tf * (self.k1 + 1.0) / (tf + self.k1 * norm)
                    })
                    .sum()
            })
            .collect()
    }
}

/// Min-max to `[0, 1]`; a constant vector maps to zeros.
#[must_use]
pub fn normalize(values: &[f64]) -> Vec<f64> {
    let (min, max) = values
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(*v), hi.max(*v))
        });
    if max.partial_cmp(&min) != Some(std::cmp::Ordering::Greater) {
        return vec![0.0; values.len()];
    }
    values.iter().map(|v| (v - min) / (max - min)).collect()
}

/// The blend of §5.2.
///
/// `query_empty` drops the BM25 half; `sparse` drops the `PageRank` half.
#[must_use]
pub fn blend(pagerank: &[f64], bm25: &[f64], query_empty: bool, sparse: bool) -> Vec<f64> {
    blend_with(pagerank, bm25, query_empty, sparse, &Params::default())
}

/// [`blend`] with the shares in `params`.
#[must_use]
pub fn blend_with(
    pagerank: &[f64],
    bm25: &[f64],
    query_empty: bool,
    sparse: bool,
    params: &Params,
) -> Vec<f64> {
    let page = normalize(pagerank);
    let text = normalize(bm25);
    page.iter()
        .zip(&text)
        .map(|(p, t)| {
            if query_empty {
                *p
            } else if sparse {
                *t
            } else {
                params.pagerank_share * p + params.bm25_share * t
            }
        })
        .collect()
}

/// Index of each path, for callers building edges from names.
#[must_use]
pub fn index_of(paths: &[String]) -> BTreeMap<&str, usize> {
    paths
        .iter()
        .enumerate()
        .map(|(i, p)| (p.as_str(), i))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dense, independent implementation of the same definition: build the
    /// transition matrix explicitly and iterate until it stops moving.
    fn reference(n: usize, edges: &[Edge], p: &[f64]) -> Vec<f64> {
        let mut m = vec![vec![0.0; n]; n];
        let mut out = vec![0.0; n];
        for e in edges {
            out[e.from] += e.weight;
        }
        for e in edges {
            m[e.to][e.from] += e.weight / out[e.from];
        }
        let mut r = p.to_vec();
        for _ in 0..200 {
            let dangling: f64 = (0..n).filter(|i| out[*i] == 0.0).map(|i| r[i]).sum();
            let mut next = vec![0.0; n];
            for i in 0..n {
                let flow: f64 = (0..n).map(|j| m[i][j] * r[j]).sum();
                next[i] = DAMPING * (flow + dangling * p[i]) + (1.0 - DAMPING) * p[i];
            }
            r = next;
        }
        r
    }

    /// A deterministic pseudo-random graph, so the test is repeatable.
    fn graph(n: usize, per_node: usize) -> Vec<Edge> {
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut edges = Vec::new();
        for from in 0..n {
            // Every tenth file has no outgoing edges: dangling mass.
            if from % 10 == 9 {
                continue;
            }
            for _ in 0..per_node {
                let to = usize::try_from(next() % n as u64).unwrap();
                if to != from {
                    edges.push(Edge {
                        from,
                        to,
                        weight: 1.0,
                    });
                }
            }
        }
        edges
    }

    /// T-CTX-008: a 500-file graph agrees with the reference to 1e-4.
    #[test]
    fn pagerank_matches_an_independent_implementation_on_500_files() {
        let n = 500;
        let edges = graph(n, 6);
        let p = personalization(n, Some(7), &[11, 12, 13]);
        let fast = pagerank(n, &edges, &p);
        let slow = reference(n, &edges, &p);
        let worst = fast
            .iter()
            .zip(&slow)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        assert!(worst < 1e-4, "largest difference {worst}");
        assert!(
            (fast.iter().sum::<f64>() - 1.0).abs() < 1e-9,
            "scores sum to one"
        );
    }

    #[test]
    fn the_current_file_and_its_neighbours_outrank_strangers() {
        // 0 → 1 → 2, and 3, 4 on their own.
        let edges = vec![
            Edge {
                from: 0,
                to: 1,
                weight: 1.0,
            },
            Edge {
                from: 1,
                to: 2,
                weight: 1.0,
            },
        ];
        let p = personalization(5, Some(0), &[]);
        let r = pagerank(5, &edges, &p);
        assert!(r[0] > r[3] && r[1] > r[3] && r[2] > r[3], "{r:?}");
        assert!((r[3] - r[4]).abs() < 1e-12);
    }

    #[test]
    fn personalization_sums_to_one_and_weights_the_focus() {
        let p = personalization(10, Some(2), &[5, 5, 6]);
        assert!((p.iter().sum::<f64>() - 1.0).abs() < 1e-12);
        assert!(p[2] > p[5] && p[5] > p[0]);
        assert!(
            (p[5] - p[6]).abs() < 1e-12,
            "a duplicate does not count twice"
        );
        // Nothing in focus: uniform.
        let p = personalization(4, None, &[]);
        assert!(p.iter().all(|v| (v - 0.25).abs() < 1e-12));
        assert!(personalization(0, None, &[]).is_empty());
    }

    #[test]
    fn tokens_split_words_cases_and_digits() {
        assert_eq!(tokens("parseHTTPRequest"), ["parse", "http", "request"]);
        assert_eq!(tokens("snake_case_name"), ["snake", "case", "name"]);
        assert_eq!(
            tokens("src/util/text2.rs"),
            ["src", "util", "text", "2", "rs"]
        );
        assert_eq!(tokens("  "), Vec::<String>::new());
        assert_eq!(tokens("Ünïcode Wörds"), ["ünïcode", "wörds"]);
    }

    fn doc(names: &str, path: &str, signatures: &str, docs: &str) -> Document {
        Document {
            names: names.into(),
            path: path.into(),
            signatures: signatures.into(),
            docs: docs.into(),
        }
    }

    #[test]
    fn bm25_prefers_a_name_match_to_a_doc_mention() {
        let docs = vec![
            doc(
                "Parser parse",
                "src/parser.rs",
                "pub fn parse()",
                "turns text into a tree",
            ),
            doc(
                "render",
                "src/view.rs",
                "fn render()",
                "does not parse anything but mentions parser once",
            ),
            doc("helper", "src/util.rs", "fn helper()", ""),
        ];
        let corpus = Corpus::new(&docs);
        let s = corpus.score("parser");
        assert!(s[0] > s[1] && s[1] > s[2], "{s:?}");
        assert_eq!(s[2], 0.0);
        assert_eq!(corpus.score("")[0], 0.0);
        assert_eq!(corpus.score("zzz-no-such-word"), vec![0.0; 3]);
    }

    #[test]
    fn a_rare_word_counts_for_more_than_a_common_one() {
        let docs: Vec<Document> = (0..20)
            .map(|i| doc(&format!("common thing{i}"), "x.rs", "", ""))
            .chain(std::iter::once(doc("common rarely", "y.rs", "", "")))
            .collect();
        let corpus = Corpus::new(&docs);
        let rare = corpus.score("rarely")[20];
        let common = corpus.score("common")[20];
        assert!(rare > common, "{rare} vs {common}");
    }

    #[test]
    fn normalisation_and_the_blend() {
        assert_eq!(normalize(&[2.0, 4.0, 6.0]), [0.0, 0.5, 1.0]);
        assert_eq!(normalize(&[3.0, 3.0]), [0.0, 0.0]);
        assert!(normalize(&[]).is_empty());
        let page = [1.0, 0.0, 0.5];
        let text = [0.0, 1.0, 0.5];
        let both = blend(&page, &text, false, false);
        assert!((both[0] - 0.55).abs() < 1e-12 && (both[1] - 0.45).abs() < 1e-12);
        assert_eq!(blend(&page, &text, true, false), [1.0, 0.0, 0.5]);
        assert_eq!(blend(&page, &text, false, true), [0.0, 1.0, 0.5]);
    }
}
