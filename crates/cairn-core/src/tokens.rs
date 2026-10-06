//! Token estimation (SPEC §4.8's fallback), shared by everything that has to
//! decide whether something fits before a provider has counted it.

/// §4.8's fallback estimator: `ceil(chars / 4)` for Latin, `ceil(bytes / 3)`
/// for CJK and code-heavy text, documented as ±15%.
///
/// The section names the two formulas but not the rule that picks between
/// them, so the predicate is stated here (and recorded in §16.5): Latin means
/// ASCII *and* at least 80% of the characters are letters, digits or spaces.
/// Prose sits near 95%; source code, with its operators, brackets and
/// newlines, sits nearer 70% — which is exactly the distinction §4.8's two
/// formulas are drawn along, since a byte of code is worth a token whether or
/// not it is readable as a word.
///
/// A `Tokenizer` the registry knows (§4.9) is always preferred: this runs when
/// `tokenizer` is `unknown` or the provider has no counter to ask.
#[must_use]
pub fn estimate_tokens(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let tokens = if is_latin_prose(text) {
        u64::try_from(text.chars().count())
            .unwrap_or(u64::MAX)
            .div_ceil(4)
    } else {
        u64::try_from(text.len()).unwrap_or(u64::MAX).div_ceil(3)
    };
    u32::try_from(tokens).unwrap_or(u32::MAX)
}

fn is_latin_prose(text: &str) -> bool {
    let mut total = 0_u64;
    let mut wordish = 0_u64;
    for ch in text.chars() {
        if !ch.is_ascii() {
            return false;
        }
        total += 1;
        if ch.is_ascii_alphanumeric() || ch == ' ' {
            wordish += 1;
        }
    }
    // `wordish / total >= 0.8`, without floats.
    total > 0 && wordish * 5 >= total * 4
}
