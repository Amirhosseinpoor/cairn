//! SPEC §4.8 — token and cost accounting.
//!
//! Two numbers decide what a turn costs: how many tokens it used, and what
//! those tokens are worth. §4.8 gives a formula for the second and a fallback
//! for the first, and REQ-PROV-011/012 say what to do when either is missing —
//! estimate and flag it, or price it `null` rather than guess.

use cairn_core::message::Usage;
use cairn_core::message::{Block, Message};
use cairn_core::registry::Pricing;

use crate::types::{ModelRequest, TokenCount, ToolSpec};

pub use cairn_core::tokens::estimate_tokens;

/// Estimate a whole prompt — message text plus the model-facing tool list.
///
/// Per-adapter framing (§4.4's role wrappers, stop sequences) and image tiles
/// are not counted: the first is provider-specific and the second is priced by
/// the provider, not by characters. Both are inside §4.8's ±15%.
#[must_use]
pub fn estimate_request(req: &ModelRequest) -> TokenCount {
    let mut tokens: u64 = 0;
    for message in &req.messages {
        tokens = tokens.saturating_add(estimate_message(message));
    }
    for tool in &req.tools {
        tokens = tokens.saturating_add(estimate_tool(tool));
    }
    // `max_tokens` is a ceiling the model may reach, so a prompt that would be
    // truncated at it must not be reported as smaller than the request.
    tokens = tokens.saturating_add(u64::from(req.max_tokens));
    Usage::estimate(u32::try_from(tokens).unwrap_or(u32::MAX))
}

fn estimate_message(message: &Message) -> u64 {
    message
        .blocks
        .iter()
        .map(estimate_block)
        .fold(0_u64, u64::saturating_add)
}

fn estimate_block(block: &Block) -> u64 {
    match block {
        Block::Text { text }
        | Block::Reasoning { text, .. }
        | Block::ThinkingPlaceholder { text } => u64::from(estimate_tokens(text)),
        Block::ToolCall { name, input, .. } => {
            u64::from(estimate_tokens(name)).saturating_add(estimate_json(input))
        }
        Block::ToolResult { content, .. } => content
            .iter()
            .map(estimate_block)
            .fold(0_u64, u64::saturating_add),
        // An image contributes tokens the provider decides (§4.2's vision
        // row); counting base64 characters as tokens would report a picture
        // as tens of thousands of tokens.
        Block::Image { .. } => 0,
    }
}

fn estimate_tool(tool: &ToolSpec) -> u64 {
    u64::from(estimate_tokens(&tool.name))
        .saturating_add(u64::from(estimate_tokens(&tool.description)))
        .saturating_add(estimate_json(&tool.input_schema))
}

fn estimate_json(value: &serde_json::Value) -> u64 {
    u64::from(estimate_tokens(&value.to_string()))
}

/// REQ-PROV-012's cost, in dollars.
///
/// `(input - cache_read) * p_in + cache_read * p_cache_read + cache_write * p_cache_write + output * p_out`,
/// every figure per million tokens.
///
/// `None` means the registry has no price (§4.9 allows `null`), and that is
/// the whole answer: a cost of `null` is what the UI prints as `cost: n/a`,
/// where a zero would read as "this was free".
///
/// The `estimated` flag on `usage` is deliberately *not* consulted — an
/// estimate is still a cost, just one that REQ-PROV-011 says must be flagged
/// as such until the provider reports real numbers.
#[must_use]
pub fn cost_usd(usage: &Usage, pricing: &Pricing) -> Option<f64> {
    let price_in = pricing.input_per_mtok?;
    let price_out = pricing.output_per_mtok?;
    let price_cache_read = pricing.cache_read_per_mtok.unwrap_or(0.0);
    let price_cache_write = pricing.cache_write_per_mtok.unwrap_or(0.0);

    // §4.8 writes `input_tokens - cache_read`, i.e. `input` is inclusive of
    // the cached prefix. A provider that reports otherwise must not produce a
    // negative cost, so the subtraction saturates.
    let fresh = usage.input.saturating_sub(usage.cache_read);

    let per_million = f64::from(fresh) * price_in
        + f64::from(usage.cache_read) * price_cache_read
        + f64::from(usage.cache_write) * price_cache_write
        + f64::from(usage.output) * price_out;
    Some(per_million / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Capabilities;
    use cairn_core::registry::{bundled, ModelEntry};

    /// §4.8's Latin branch: four characters to the token.
    #[test]
    fn latin_prose_is_four_characters_a_token() {
        let text = "hello world, this is a sentence.";
        assert_eq!(text.chars().count(), 32);
        assert_eq!(estimate_tokens(text), 8, "ceil(32 / 4)");
        assert_eq!(estimate_tokens("a"), 1, "a partial token still counts");
        assert_eq!(estimate_tokens("the quick brown fox"), 5, "ceil(19 / 4)");
    }

    /// §4.8's other branch: three *bytes*, which for CJK is also three bytes
    /// per character — four characters would otherwise be estimated as one.
    #[test]
    fn cjk_is_three_bytes_a_token() {
        let text = "你好，世界";
        assert_eq!(text.chars().count(), 5);
        assert_eq!(text.len(), 15, "5 chars x 3 bytes");
        assert_eq!(estimate_tokens(text), 5, "ceil(15 / 3), not ceil(5 / 4)");
        assert!(text.chars().all(|c| !c.is_ascii()));
    }

    /// §4.8's "code-heavy": operators and brackets drop the text out of the
    /// prose branch, so the byte formula applies.
    #[test]
    fn source_code_leaves_the_prose_branch() {
        let code = "fn main() { let x: u32 = 1 + 2; println!(\"{}\", x); }";
        let wordish = code
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == ' ')
            .count();
        assert!(
            wordish * 5 < code.len() * 4,
            "this snippet should not read as prose ({wordish}/{})",
            code.len()
        );
        assert_eq!(
            estimate_tokens(code),
            u32::try_from(code.len().div_ceil(3)).expect("small"),
            "ceil(len / 3)"
        );

        // An empty prompt costs nothing rather than one token.
        assert_eq!(estimate_tokens(""), 0);
    }

    /// An estimate is only ever an estimate (REQ-PROV-011).
    #[test]
    fn an_estimate_is_flagged_as_one() {
        let usage = Usage::estimate(estimate_tokens("four score and seven years ago"));
        assert!(usage.estimated, "REQ-PROV-011 requires the flag");
        assert_eq!(usage.output, 0, "nothing has been generated yet");

        let reported = Usage::reported(10, 5, 0, 0);
        assert!(!reported.estimated);
    }

    /// §4.8's formula, worked through with §4.9's real prices.
    #[test]
    fn cost_follows_the_section_4_8_formula() {
        let sonnet = &bundled().models["anthropic/claude-sonnet-4-5"];
        assert_eq!(sonnet.pricing.input_per_mtok, Some(3.0));
        assert_eq!(sonnet.pricing.output_per_mtok, Some(15.0));
        assert_eq!(sonnet.pricing.cache_read_per_mtok, Some(0.3));
        assert_eq!(sonnet.pricing.cache_write_per_mtok, Some(3.75));

        // (1000 - 200) * 3.0 + 200 * 0.3 + 100 * 3.75 + 500 * 15.0
        //   = 2400 + 60 + 375 + 7500 = 10335 per million.
        let usage = Usage::reported(1000, 500, 200, 100);
        let cost = cost_usd(&usage, &sonnet.pricing).expect("priced");
        assert!((cost - 0.010_335).abs() < 1e-12, "got {cost}");
    }

    /// REQ-PROV-012, T-PROV-012: an unknown price is `null`, never a zero.
    #[test]
    fn an_unpriced_model_costs_null() {
        let codex = &bundled().models["openai/gpt-5.1-codex"];
        assert!(
            !codex.pricing.is_fully_priced(),
            "§4.9 ships gpt-5.1-codex unpriced"
        );
        let usage = Usage::reported(1_000_000, 1_000_000, 0, 0);
        assert_eq!(cost_usd(&usage, &codex.pricing), None);
    }

    /// A local model is priced at zero — which is a number, and must not be
    /// confused with the `null` above.
    #[test]
    fn a_free_model_costs_zero() {
        let local = &bundled().models["ollama/qwen2.5-coder:14b"];
        let usage = Usage::reported(1_000_000, 1_000_000, 0, 0);
        assert_eq!(cost_usd(&usage, &local.pricing), Some(0.0));
    }

    /// §4.9 omits `cache_read_per_mtok` when a model has no cache pricing; an
    /// omitted tier is free, not an error.
    #[test]
    fn a_missing_cache_tier_is_free_not_an_error() {
        let pricing = Pricing {
            input_per_mtok: Some(1.0),
            output_per_mtok: Some(2.0),
            cache_read_per_mtok: None,
            cache_write_per_mtok: None,
        };
        let usage = Usage::reported(500_000, 250_000, 500_000, 100_000);
        // 0 * 1.0 + 500000 * 0 + 100000 * 0 + 250000 * 2.0 = 0.5 per million
        let cost = cost_usd(&usage, &pricing).expect("priced");
        assert!((cost - 0.5).abs() < 1e-12, "got {cost}");
    }

    /// A provider that reports `cache_read > input` (or a prompt assembled
    /// from a cache only) must not produce a negative number.
    #[test]
    fn cached_tokens_cannot_exceed_input() {
        let pricing = Pricing {
            input_per_mtok: Some(1.0),
            output_per_mtok: Some(1.0),
            cache_read_per_mtok: Some(1.0),
            cache_write_per_mtok: None,
        };
        let usage = Usage {
            input: 100,
            output: 0,
            cache_read: 500,
            cache_write: 0,
            estimated: false,
        };
        let cost = cost_usd(&usage, &pricing).expect("priced");
        assert!(cost >= 0.0, "got {cost}");
        assert!((cost - 0.000_5).abs() < 1e-12, "got {cost}");
    }

    /// §4.9's `capabilities` object is seven booleans; §3.4's `Capabilities`
    /// adds the two limits the same row carries beside it. T-PROV-003's
    /// "`capabilities()` equals registry row" is this conversion.
    #[test]
    fn capabilities_are_the_registry_row_plus_its_limits() {
        let reg = bundled();
        let model: &ModelEntry = &reg.models["anthropic/claude-sonnet-4-5"];
        let caps = Capabilities::from_entry(model);

        assert_eq!(caps.tool_calling, model.capabilities.tool_calling);
        assert_eq!(caps.streaming, model.capabilities.streaming);
        assert_eq!(caps.reasoning, model.capabilities.reasoning);
        assert_eq!(caps.prompt_cache, model.capabilities.prompt_cache);
        assert_eq!(caps.vision, model.capabilities.vision);
        assert_eq!(
            caps.parallel_tool_calls,
            model.capabilities.parallel_tool_calls
        );
        assert_eq!(
            caps.json_schema_strict,
            model.capabilities.json_schema_strict
        );
        assert_eq!(caps.max_context, model.context_window);
        assert_eq!(caps.max_output, model.max_output);
        assert_eq!(caps.max_context, 200_000);
        assert_eq!(caps.max_output, 64_000);

        // §4.6 branches on this flag; a default that read "false" here would
        // silently disable native tool calls for every adapter.
        assert!(caps.tool_calling, "sonnet calls tools natively");
        assert!(caps.streaming, "§3.4 makes streaming the one guarantee");
        assert!(!Capabilities::baseline().tool_calling);
    }

    /// The estimator feeds `Provider::count_tokens`, so a prompt's size moves
    /// with its content rather than staying flat.
    #[test]
    fn an_estimated_request_grows_with_the_conversation() {
        let short = ModelRequest::new(
            "ollama/qwen2.5-coder:14b",
            vec![Message::user("hi", 0)],
            1024,
        );
        let long = ModelRequest::new(
            "ollama/qwen2.5-coder:14b",
            vec![Message::user("word ".repeat(400), 0)],
            1024,
        );
        let a = estimate_request(&short);
        let b = estimate_request(&long);
        assert!(a.estimated && b.estimated);
        assert!(b.input > a.input, "{b:?} should exceed {a:?}");

        // Tools and their schemas are part of the prompt too.
        let mut with_tools = long;
        with_tools.tools.push(ToolSpec {
            name: "read".to_string(),
            description: "Read a file from the workspace.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["path"],
                "properties": { "path": { "type": "string" } }
            }),
        });
        assert!(estimate_request(&with_tools).input > b.input);
    }
}
