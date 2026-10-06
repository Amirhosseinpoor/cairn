//! §4.4 adapters: five thin shapings over the landed trait and transport.
//!
//! Each adapter is three things: a §4.9 identity (provider key, canonical
//! model id, the registry rows they resolve to), §4.4's request shaping as
//! pure functions (snapshot-tested, no network), and the [`Provider`] trait
//! methods. Everything after shaping — the client, status mapping, SSE/NDJSON
//! framing, decoding — is [`transport`].
//!
//! Construction never fails: a model the registry does not know, a key that
//! is not there, or a `base_url` that is not a URL are [`ProviderHealth`]
//! states, and `stream()` refuses to run in any of them. A user-defined
//! `models.<id>` (REQ-PROV-013's escape hatch) does not resolve against the
//! bare registry — bridging it is the `run` wiring's job, which owns the full
//! `Config`.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::FutureExt;
use serde_json::{Map, Value};

use cairn_core::cancel::CancellationToken;
use cairn_core::message::{Block, MediaType, Message, Role};
use cairn_core::registry::{ProviderKind, Registry};

use crate::accounting::estimate_request;
use crate::error::{ProviderError, ProviderFault};
use crate::fallback::fallback_section;
use crate::transport::{self, Request};
use crate::types::{
    Capabilities, ModelRequest, ProviderHealth, ProviderId, StreamEvent, TokenCount, ToolSpec,
};
use crate::Provider;

/// Which §4.4 column shapes the request. vLLM and the proxies reuse the
/// `OpenAI` shape — that is what "OpenAI-compatible" means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Anthropic,
    Openai,
    Ollama,
}

impl Shape {
    /// The path appended to the provider's `base_url` (§4.4).
    fn path(self) -> &'static str {
        match self {
            Shape::Anthropic => "/v1/messages",
            Shape::Openai => "/v1/chat/completions",
            Shape::Ollama => "/api/chat",
        }
    }
}

/// The resolved half of an adapter: everything `stream()` needs that comes
/// from the registry rather than the call.
#[derive(Debug, Clone)]
struct Resolved {
    /// Canonical registry id, e.g. `anthropic/claude-sonnet-4-5`.
    model: String,
    /// Registry `base_url`, unless the caller overrode it
    /// (`providers.<id>.base_url`, `""` meaning the default).
    base_url: String,
    auth_name: String,
    auth_prefix: String,
    capabilities: Capabilities,
}

/// Shared by all five adapters: identity, credentials, the §4.9 rows, the
/// client, and the side channel the §4.5 retry loop will read.
#[derive(Debug)]
struct Core {
    provider: ProviderId,
    kind: ProviderKind,
    shape: Shape,
    /// The model id the caller asked for — what `UnknownModel` names. It
    /// differs from `resolved.model` when the caller passed an alias.
    wanted: String,
    /// `None` until §4.10's lookup answers (env today; keychain and files
    /// with the `auth` wiring).
    key: Option<String>,
    resolved: Option<Resolved>,
    /// Built once: a bad `ca_bundle` is a `Misconfigured` health state, not
    /// a per-call surprise.
    client: Result<reqwest::Client, ProviderError>,
    /// The fault behind a mid-stream `Finish { stop: Error }`, if any.
    last_error: Arc<Mutex<Option<ProviderError>>>,
    /// §4.2's auto-detect probe, remembered: set once a compatible server
    /// has rejected native `tools`, after which every call is shaped for the
    /// §4.6 prompt fallback and `capabilities()` says so.
    tools_rejected: AtomicBool,
}

impl Core {
    fn new(
        kind: ProviderKind,
        shape: Shape,
        model: &str,
        registry: &Registry,
        key: Option<String>,
        ca_bundle: Option<&Path>,
        base_url: Option<String>,
    ) -> Self {
        let resolved = registry.resolve_with_provider(model).map(|found| {
            let (auth_name, auth_prefix) = found.provider.auth_parts();
            let base_url = match base_url {
                Some(url) if !url.is_empty() => url,
                _ => found.provider.base_url.clone(),
            };
            Resolved {
                model: found.id.to_string(),
                base_url,
                auth_name: auth_name.to_string(),
                auth_prefix: auth_prefix.to_string(),
                capabilities: Capabilities::from_entry(found.model),
            }
        });
        Self {
            provider: ProviderId::new(provider_key(kind)),
            kind,
            shape,
            wanted: model.to_string(),
            key: key.filter(|key| !key.is_empty()),
            resolved,
            client: transport::build_client(ca_bundle),
            last_error: Arc::new(Mutex::new(None)),
            tools_rejected: AtomicBool::new(false),
        }
    }

    /// What this adapter can do *now*: the registry row, minus native tool
    /// calling once the server has refused it.
    fn capabilities(&self) -> Capabilities {
        let mut caps = self
            .resolved
            .as_ref()
            .map_or(Capabilities::baseline(), |found| found.capabilities);
        if self.tools_rejected.load(Ordering::Relaxed) {
            caps.tool_calling = false;
        }
        caps
    }

    /// Non-network capability probe (§3.4): credentials present, model id
    /// known, configuration usable — in that order, so the first thing a new
    /// user must fix is what gets reported.
    fn health(&self) -> ProviderHealth {
        let Some(resolved) = &self.resolved else {
            return ProviderHealth::UnknownModel;
        };
        if reqwest::Url::parse(&resolved.base_url).is_err() {
            return ProviderHealth::Misconfigured {
                reason: format!("base_url `{}` is not a valid URI", resolved.base_url),
            };
        }
        if let Err(error) = &self.client {
            return ProviderHealth::Misconfigured {
                reason: error.message.clone(),
            };
        }
        // §4.10: only Ollama's local server is usable with no key at all.
        if self.key.is_none() && self.kind != ProviderKind::Ollama {
            return ProviderHealth::NoCredentials;
        }
        ProviderHealth::Ready
    }

    fn check_ready(&self) -> Result<&Resolved, ProviderError> {
        match self.health() {
            ProviderHealth::Ready => {}
            ProviderHealth::NoCredentials => {
                return Err(ProviderError::new(
                    ProviderFault::Auth,
                    format!(
                        "no API key for provider `{}`; set {} or run 'cairn auth login `{}`'",
                        self.provider,
                        env_key_name(self.provider.as_str()),
                        self.provider,
                    ),
                ));
            }
            ProviderHealth::UnknownModel => {
                return Err(ProviderError::new(
                    ProviderFault::NoModel,
                    format!("unknown model `{}`", self.wanted),
                ));
            }
            ProviderHealth::Misconfigured { reason } => {
                // Deterministic, so never retried: `BadRequest` is the
                // non-retryable fault with no narrower meaning here.
                return Err(ProviderError::new(ProviderFault::BadRequest, reason));
            }
        }
        self.resolved
            .as_ref()
            .ok_or_else(|| ProviderError::new(ProviderFault::NoModel, "unknown model"))
    }

    fn headers(&self, resolved: &Resolved) -> Vec<(String, String)> {
        let mut headers = vec![("accept".to_string(), "text/event-stream".to_string())];
        if let Some(key) = &self.key {
            let value = if resolved.auth_prefix.is_empty() {
                key.clone()
            } else {
                format!("{} {key}", resolved.auth_prefix)
            };
            headers.push((resolved.auth_name.clone(), value));
        }
        if self.shape == Shape::Anthropic {
            // Pinned to the version §4.4 was written against, not "latest":
            // Anthropic versions by header, and an unpinned header drifts.
            headers.push(("anthropic-version".to_string(), "2023-06-01".to_string()));
        }
        headers
    }

    fn stream_impl(
        &self,
        req: ModelRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<BoxStream<'static, StreamEvent>, ProviderError>> {
        async move {
            let native = self.capabilities().tool_calling;
            let first = self.post(&req, native, cancel.clone()).await;
            // §4.2, "auto-detect" for an OpenAI-compatible server: the first
            // call that carries tools *is* the probe. A 400 that blames the
            // tools means the server has no native tool calling, so the same
            // call is sent again shaped for the §4.6 prompt fallback, once,
            // and the answer is remembered for the rest of the process.
            match first {
                Err(error)
                    if native
                        && self.kind == ProviderKind::OpenaiCompatible
                        && !req.tools.is_empty()
                        && rejects_native_tools(&error) =>
                {
                    self.tools_rejected.store(true, Ordering::Relaxed);
                    tracing::info!(
                        event = "provider.tools_probe",
                        provider = self.provider.as_str(),
                        "server rejected native tools; using the prompt fallback"
                    );
                    self.post(&req, false, cancel).await
                }
                other => other,
            }
        }
        .boxed()
    }

    async fn post(
        &self,
        req: &ModelRequest,
        native_tools: bool,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let resolved = self.check_ready()?;
        let client = self.client.as_ref().map_err(Clone::clone)?;
        let url = format!(
            "{}{}",
            resolved.base_url.trim_end_matches('/'),
            self.shape.path()
        );
        let request = Request {
            url,
            headers: self.headers(resolved),
            body: shape_body(self.shape, &resolved.model, native_tools, req),
        };
        transport::post_events(
            client,
            self.provider.as_str(),
            request,
            self.kind,
            cancel,
            Arc::clone(&self.last_error),
        )
        .await
    }

    fn count_impl(req: &ModelRequest) -> BoxFuture<'_, Result<TokenCount, ProviderError>> {
        // §4.8's estimator, never a network round trip (the trait's own
        // contract): counting is pure arithmetic on the request.
        async move { Ok(estimate_request(req)) }.boxed()
    }
}

/// Whether a 400 is the server refusing the `tools` parameter itself (as
/// opposed to any other bad request, which must not be silently retried).
fn rejects_native_tools(error: &ProviderError) -> bool {
    if error.fault != ProviderFault::BadRequest {
        return false;
    }
    let message = error.message.to_ascii_lowercase();
    [
        "tools",
        "tool_choice",
        "function call",
        "functions",
        "unknown field",
        "unrecognized",
        "unsupported parameter",
        "extra inputs",
        "not supported",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

/// The §4.9 provider key for a kind: the name `id()` reports and
/// `CAIRN_<PROVIDER>_API_KEY` derives from.
fn provider_key(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Openai => "openai",
        ProviderKind::OpenaiCompatible => "openai_compatible",
        ProviderKind::Ollama => "ollama",
        ProviderKind::Vllm => "vllm",
    }
}
/// §4.10's first two lookup steps: `CAIRN_<PROVIDER>_API_KEY`, then the
/// provider-standard variable from the registry (`ANTHROPIC_API_KEY`, …).
/// Empty strings do not count — an exported-but-empty variable is the same
/// as an absent one. Keychain and files (steps 3–5) arrive with the `auth`
/// wiring; this is what adapter construction uses until then.
#[must_use]
pub fn env_key(provider_key: &str, standard: Option<&str>) -> Option<String> {
    std::env::var(env_key_name(provider_key))
        .ok()
        .filter(|key| !key.is_empty())
        .or_else(|| {
            standard.and_then(|name| std::env::var(name).ok().filter(|key| !key.is_empty()))
        })
}

/// The `CAIRN_<PROVIDER>_API_KEY` spelling for a registry provider key.
#[must_use]
pub fn env_key_name(provider_key: &str) -> String {
    let mut name = String::from("CAIRN_");
    for byte in provider_key.bytes() {
        name.push(if byte.is_ascii_alphanumeric() {
            byte.to_ascii_uppercase() as char
        } else {
            '_'
        });
    }
    name.push_str("_API_KEY");
    name
}

// ------------------------------------------------------------ the five kinds

/// Anthropic (`/v1/messages`), shaped per §4.4's first column.
#[derive(Debug)]
pub struct AnthropicAdapter {
    core: Core,
}

impl AnthropicAdapter {
    /// `base_url` overrides the registry default (`""` keeps it); `key`
    /// should come from [`env_key`].
    #[must_use]
    pub fn new(
        model: &str,
        registry: &Registry,
        key: Option<String>,
        ca_bundle: Option<&Path>,
        base_url: Option<String>,
    ) -> Self {
        Self {
            core: Core::new(
                ProviderKind::Anthropic,
                Shape::Anthropic,
                model,
                registry,
                key,
                ca_bundle,
                base_url,
            ),
        }
    }
}

impl Provider for AnthropicAdapter {
    fn id(&self) -> &ProviderId {
        &self.core.provider
    }

    fn capabilities(&self) -> Capabilities {
        self.core.capabilities()
    }

    fn stream(
        &self,
        req: ModelRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<BoxStream<'static, StreamEvent>, ProviderError>> {
        self.core.stream_impl(req, cancel)
    }

    fn count_tokens<'a>(
        &'a self,
        req: &'a ModelRequest,
    ) -> BoxFuture<'a, Result<TokenCount, ProviderError>> {
        Core::count_impl(req)
    }

    fn health(&self) -> ProviderHealth {
        self.core.health()
    }

    fn take_last_error(&self) -> Option<ProviderError> {
        self.core.last_error.lock().expect("fault record").take()
    }

    fn record_last_error(&self, error: ProviderError) {
        *self.core.last_error.lock().expect("fault record") = Some(error);
    }
}

/// `OpenAI` (`/v1/chat/completions`), shaped per §4.4's second column.
#[derive(Debug)]
pub struct OpenaiAdapter {
    core: Core,
}

impl OpenaiAdapter {
    /// See [`AnthropicAdapter::new`].
    #[must_use]
    pub fn new(
        model: &str,
        registry: &Registry,
        key: Option<String>,
        ca_bundle: Option<&Path>,
        base_url: Option<String>,
    ) -> Self {
        Self {
            core: Core::new(
                ProviderKind::Openai,
                Shape::Openai,
                model,
                registry,
                key,
                ca_bundle,
                base_url,
            ),
        }
    }
}

impl Provider for OpenaiAdapter {
    fn id(&self) -> &ProviderId {
        &self.core.provider
    }

    fn capabilities(&self) -> Capabilities {
        self.core.capabilities()
    }

    fn stream(
        &self,
        req: ModelRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<BoxStream<'static, StreamEvent>, ProviderError>> {
        self.core.stream_impl(req, cancel)
    }

    fn count_tokens<'a>(
        &'a self,
        req: &'a ModelRequest,
    ) -> BoxFuture<'a, Result<TokenCount, ProviderError>> {
        Core::count_impl(req)
    }

    fn health(&self) -> ProviderHealth {
        self.core.health()
    }

    fn take_last_error(&self) -> Option<ProviderError> {
        self.core.last_error.lock().expect("fault record").take()
    }

    fn record_last_error(&self, error: ProviderError) {
        *self.core.last_error.lock().expect("fault record") = Some(error);
    }
}

/// An `OpenAI`-compatible proxy (Groq, Together, LM Studio, …): the `OpenAI`
/// shape against a caller-supplied URL.
///
/// There is no registry entry to resolve — a proxy's models and address are
/// the user's own — so the model name is used as-is and the capabilities are
/// declared by the caller (from `models.<id>`). §4.2's auto-detect probe
/// refines `tool_calling` later; until it does, the declared value is what
/// §4.6 branches on.
#[derive(Debug)]
pub struct OpenaiCompatibleAdapter {
    core: Core,
}

impl OpenaiCompatibleAdapter {
    /// `base_url` is required (a proxy has no registry default);
    /// `capabilities` declares what the remote model can do.
    #[must_use]
    pub fn new(
        model: &str,
        base_url: String,
        key: Option<String>,
        capabilities: Capabilities,
        ca_bundle: Option<&Path>,
    ) -> Self {
        let model = model.to_string();
        Self {
            core: Core {
                provider: ProviderId::new("openai_compatible"),
                kind: ProviderKind::OpenaiCompatible,
                shape: Shape::Openai,
                wanted: model.clone(),
                key: key.filter(|key| !key.is_empty()),
                resolved: Some(Resolved {
                    model,
                    base_url,
                    auth_name: "Authorization".to_string(),
                    auth_prefix: "Bearer".to_string(),
                    capabilities,
                }),
                client: transport::build_client(ca_bundle),
                last_error: Arc::new(Mutex::new(None)),
                tools_rejected: AtomicBool::new(false),
            },
        }
    }
}

impl Provider for OpenaiCompatibleAdapter {
    fn id(&self) -> &ProviderId {
        &self.core.provider
    }

    fn capabilities(&self) -> Capabilities {
        self.core.capabilities()
    }

    fn stream(
        &self,
        req: ModelRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<BoxStream<'static, StreamEvent>, ProviderError>> {
        self.core.stream_impl(req, cancel)
    }

    fn count_tokens<'a>(
        &'a self,
        req: &'a ModelRequest,
    ) -> BoxFuture<'a, Result<TokenCount, ProviderError>> {
        Core::count_impl(req)
    }

    fn health(&self) -> ProviderHealth {
        self.core.health()
    }

    fn take_last_error(&self) -> Option<ProviderError> {
        self.core.last_error.lock().expect("fault record").take()
    }

    fn record_last_error(&self, error: ProviderError) {
        *self.core.last_error.lock().expect("fault record") = Some(error);
    }
}

/// Ollama (`/api/chat`, NDJSON), shaped per §4.4's third column.
#[derive(Debug)]
pub struct OllamaAdapter {
    core: Core,
}

impl OllamaAdapter {
    /// See [`AnthropicAdapter::new`]. No key is required (§4.10).
    #[must_use]
    pub fn new(
        model: &str,
        registry: &Registry,
        key: Option<String>,
        ca_bundle: Option<&Path>,
        base_url: Option<String>,
    ) -> Self {
        Self {
            core: Core::new(
                ProviderKind::Ollama,
                Shape::Ollama,
                model,
                registry,
                key,
                ca_bundle,
                base_url,
            ),
        }
    }
}

impl Provider for OllamaAdapter {
    fn id(&self) -> &ProviderId {
        &self.core.provider
    }

    fn capabilities(&self) -> Capabilities {
        self.core.capabilities()
    }

    fn stream(
        &self,
        req: ModelRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<BoxStream<'static, StreamEvent>, ProviderError>> {
        self.core.stream_impl(req, cancel)
    }

    fn count_tokens<'a>(
        &'a self,
        req: &'a ModelRequest,
    ) -> BoxFuture<'a, Result<TokenCount, ProviderError>> {
        Core::count_impl(req)
    }

    fn health(&self) -> ProviderHealth {
        self.core.health()
    }

    fn take_last_error(&self) -> Option<ProviderError> {
        self.core.last_error.lock().expect("fault record").take()
    }

    fn record_last_error(&self, error: ProviderError) {
        *self.core.last_error.lock().expect("fault record") = Some(error);
    }
}

/// vLLM (the `OpenAI` shape against a self-hosted server), per §4.4's fourth
/// column. The bundled `base_url` is loopback; a real server comes from
/// `providers.vllm.base_url`.
#[derive(Debug)]
pub struct VllmAdapter {
    core: Core,
}

impl VllmAdapter {
    /// See [`AnthropicAdapter::new`].
    #[must_use]
    pub fn new(
        model: &str,
        registry: &Registry,
        key: Option<String>,
        ca_bundle: Option<&Path>,
        base_url: Option<String>,
    ) -> Self {
        Self {
            core: Core::new(
                ProviderKind::Vllm,
                Shape::Openai,
                model,
                registry,
                key,
                ca_bundle,
                base_url,
            ),
        }
    }
}

impl Provider for VllmAdapter {
    fn id(&self) -> &ProviderId {
        &self.core.provider
    }

    fn capabilities(&self) -> Capabilities {
        self.core.capabilities()
    }

    fn stream(
        &self,
        req: ModelRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<BoxStream<'static, StreamEvent>, ProviderError>> {
        self.core.stream_impl(req, cancel)
    }

    fn count_tokens<'a>(
        &'a self,
        req: &'a ModelRequest,
    ) -> BoxFuture<'a, Result<TokenCount, ProviderError>> {
        Core::count_impl(req)
    }

    fn health(&self) -> ProviderHealth {
        self.core.health()
    }

    fn take_last_error(&self) -> Option<ProviderError> {
        self.core.last_error.lock().expect("fault record").take()
    }

    fn record_last_error(&self, error: ProviderError) {
        *self.core.last_error.lock().expect("fault record") = Some(error);
    }
}

// ------------------------------------------------------------------ shaping

/// Shape one neutral request into one provider's JSON body (§4.4).
fn shape_body(shape: Shape, canonical: &str, tool_calling: bool, req: &ModelRequest) -> Value {
    if !req.tools.is_empty() && !tool_calling {
        // §4.6: no native `tools` parameter on this model. The definitions
        // move into the system prompt as the fallback section (a plain system
        // message, which every column already shapes), the native parameters
        // stay off, and results come back as `<tool_result>` text for the
        // extractor — so one code path shapes both worlds.
        let mut fallen = req.clone();
        let section = fallback_section(&fallen.tools);
        fallen.tools.clear();
        let turn = fallen.turn_id;
        fallen.messages.push(Message::new(
            Role::System,
            vec![Block::Text { text: section }],
            turn,
        ));
        return shape_native(shape, canonical, &fallen);
    }
    shape_native(shape, canonical, req)
}

/// The native-tool-call half of [`shape_body`].
fn shape_native(shape: Shape, canonical: &str, req: &ModelRequest) -> Value {
    let api_model = canonical.rsplit('/').next().unwrap_or(canonical);
    match shape {
        Shape::Anthropic => anthropic_body(api_model, req),
        Shape::Openai => openai_body(api_model, wants_max_completion(canonical), req),
        Shape::Ollama => ollama_body(api_model, req),
    }
}

/// The o-series reasoning models reject `max_tokens`: anything whose registry
/// name is `o` followed by a digit (`o1`, `o3`, `o4-mini`, …) gets
/// `max_completion_tokens` instead (§4.4). A name test, not a capability
/// test — §4.2's branch-on-capabilities rule is about behaviour, and this is
/// a parameter spelling.
fn wants_max_completion(canonical: &str) -> bool {
    canonical.split_once('/').is_some_and(|(_, name)| {
        let mut bytes = name.bytes();
        bytes.next() == Some(b'o') && bytes.next().is_some_and(|byte| byte.is_ascii_digit())
    })
}

fn mime(media_type: &MediaType) -> &'static str {
    match media_type {
        MediaType::Png => "image/png",
        MediaType::Jpeg => "image/jpeg",
        MediaType::Gif => "image/gif",
        MediaType::Webp => "image/webp",
    }
}

/// Anthropic's `system` parameter: the system messages' text blocks in order,
/// with the breakpoint on the last one so the whole prefix caches as one.
fn anthropic_system(messages: &[Message]) -> Vec<Value> {
    let mut texts: Vec<String> = Vec::new();
    for message in messages
        .iter()
        .filter(|message| message.role == Role::System)
    {
        for block in &message.blocks {
            if let Block::Text { text } = block {
                texts.push(text.clone());
            }
        }
    }
    let count = texts.len();
    texts
        .into_iter()
        .enumerate()
        .map(|(index, text)| {
            let mut block = Map::new();
            block.insert("type".to_string(), Value::from("text"));
            block.insert("text".to_string(), Value::from(text));
            // One breakpoint, on the last block, so the whole system prefix
            // caches as one rather than charging a cache write per message.
            if index + 1 == count {
                block.insert(
                    "cache_control".to_string(),
                    serde_json::json!({"type": "ephemeral"}),
                );
            }
            Value::Object(block)
        })
        .collect()
}

fn anthropic_message(message: &Message) -> Value {
    // System messages never reach `messages[]` — they are the `system`
    // parameter. Mapping one to `"user"` keeps its text in the call instead
    // of crashing on an adapter bug.
    let role = match message.role {
        Role::Assistant => "assistant",
        Role::System | Role::User | Role::Tool => "user",
    };
    let content: Vec<Value> = message.blocks.iter().flat_map(anthropic_block).collect();
    serde_json::json!({"role": role, "content": content})
}

fn anthropic_block(block: &Block) -> Vec<Value> {
    match block {
        Block::Text { text } => vec![serde_json::json!({"type": "text", "text": text})],
        Block::Reasoning { text, signature } => {
            let mut thinking = Map::new();
            thinking.insert("type".to_string(), Value::from("thinking"));
            thinking.insert("thinking".to_string(), Value::from(text.clone()));
            if let Some(signature) = signature {
                thinking.insert("signature".to_string(), Value::from(signature.clone()));
            }
            vec![Value::Object(thinking)]
        }
        Block::Image {
            media_type,
            data_b64,
            ..
        } => vec![serde_json::json!({
            "type": "image",
            "source": {"type": "base64", "media_type": mime(media_type), "data": data_b64},
        })],
        Block::ToolCall {
            call_id,
            name,
            input,
            ..
        } => vec![serde_json::json!({
            "type": "tool_use", "id": call_id, "name": name, "input": input,
        })],
        Block::ToolResult {
            call_id,
            content,
            is_error,
        } => {
            let mut result = Map::new();
            result.insert("type".to_string(), Value::from("tool_result"));
            result.insert("tool_use_id".to_string(), Value::from(call_id.clone()));
            result.insert(
                "content".to_string(),
                Value::Array(content.iter().flat_map(anthropic_result_block).collect()),
            );
            if *is_error {
                result.insert("is_error".to_string(), Value::from(true));
            }
            vec![Value::Object(result)]
        }
        Block::ThinkingPlaceholder { text } => {
            vec![serde_json::json!({"type": "text", "text": text})]
        }
    }
}

/// Inner blocks of a `tool_result`: text survives verbatim, reasoning keeps
/// its words (dropping them would starve the next turn), images map as
/// usual, and nested calls/results have no wire form and are skipped.
fn anthropic_result_block(block: &Block) -> Vec<Value> {
    match block {
        Block::Text { text } | Block::Reasoning { text, .. } => {
            vec![serde_json::json!({"type": "text", "text": text})]
        }
        Block::Image { .. } => anthropic_block(block),
        Block::ToolCall { .. } | Block::ToolResult { .. } | Block::ThinkingPlaceholder { .. } => {
            vec![]
        }
    }
}

fn anthropic_tool(tool: &ToolSpec) -> Value {
    serde_json::json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.input_schema,
    })
}

fn anthropic_body(api_model: &str, req: &ModelRequest) -> Value {
    let mut body = Map::new();
    body.insert("model".to_string(), Value::from(api_model));
    body.insert("max_tokens".to_string(), Value::from(req.max_tokens));
    body.insert("stream".to_string(), Value::from(true));
    let system = anthropic_system(&req.messages);
    if !system.is_empty() {
        body.insert("system".to_string(), Value::Array(system));
    }
    let messages: Vec<Value> = req
        .messages
        .iter()
        .filter(|message| message.role != Role::System)
        .map(anthropic_message)
        .collect();
    body.insert("messages".to_string(), Value::Array(messages));
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req.tools.iter().map(anthropic_tool).collect();
        body.insert("tools".to_string(), Value::Array(tools));
        body.insert(
            "tool_choice".to_string(),
            serde_json::json!({"type": "auto"}),
        );
    }
    if let Some(temperature) = req.temperature {
        body.insert("temperature".to_string(), Value::from(temperature));
    }
    Value::Object(body)
}

fn openai_tool(tool: &ToolSpec) -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.input_schema,
        },
    })
}

/// One `OpenAI`-family message. Text concatenates; images switch the content
/// to parts; reasoning has no input form on this API and is dropped (the
/// session keeps it via `ThinkingPlaceholder`, which maps as text).
fn openai_message(message: &Message) -> Value {
    let mut text = String::new();
    let mut images: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for block in &message.blocks {
        match block {
            Block::Text { text: part } | Block::ThinkingPlaceholder { text: part } => {
                text.push_str(part);
            }
            Block::Image {
                media_type,
                data_b64,
                ..
            } => images.push(serde_json::json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{};base64,{data_b64}", mime(media_type))},
            })),
            // Reasoning has no input form on this API and is dropped (the
            // session keeps it via `ThinkingPlaceholder`, which maps as
            // text); a `ToolResult` here is malformed history — results
            // travel in `tool`-role messages — and is dropped with it.
            Block::Reasoning { .. } | Block::ToolResult { .. } => {}
            Block::ToolCall {
                call_id,
                name,
                input,
                ..
            } => tool_calls.push(serde_json::json!({
                "id": call_id,
                "type": "function",
                "function": {"name": name, "arguments": input.to_string()},
            })),
        }
    }
    match message.role {
        Role::System => serde_json::json!({"role": "system", "content": text}),
        Role::User => {
            if images.is_empty() {
                serde_json::json!({"role": "user", "content": text})
            } else {
                let mut parts = vec![serde_json::json!({"type": "text", "text": text})];
                parts.extend(images);
                serde_json::json!({"role": "user", "content": parts})
            }
        }
        Role::Assistant => {
            let mut message = Map::new();
            message.insert("role".to_string(), Value::from("assistant"));
            if tool_calls.is_empty() {
                message.insert("content".to_string(), Value::from(text));
            } else if text.is_empty() {
                message.insert("content".to_string(), Value::Null);
                message.insert("tool_calls".to_string(), Value::Array(tool_calls));
            } else {
                message.insert("content".to_string(), Value::from(text));
                message.insert("tool_calls".to_string(), Value::Array(tool_calls));
            }
            Value::Object(message)
        }
        // REQ-PROV-001's shape: one `tool` message per result block.
        Role::Tool => serde_json::json!({"role": "tool", "content": text}),
    }
}

/// A `tool`-role message carries one result per block, so it fans out while
/// every other role maps one-to-one.
fn openai_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for message in messages {
        if message.role == Role::Tool {
            for block in &message.blocks {
                if let Block::ToolResult {
                    call_id, content, ..
                } = block
                {
                    let text: String = content
                        .iter()
                        .filter_map(|inner| match inner {
                            Block::Text { text }
                            | Block::Reasoning { text, .. }
                            | Block::ThinkingPlaceholder { text } => Some(text.clone()),
                            Block::Image { .. }
                            | Block::ToolCall { .. }
                            | Block::ToolResult { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    out.push(serde_json::json!({
                        "role": "tool", "tool_call_id": call_id, "content": text,
                    }));
                }
            }
        } else {
            out.push(openai_message(message));
        }
    }
    out
}

fn openai_body(api_model: &str, max_completion: bool, req: &ModelRequest) -> Value {
    let mut body = Map::new();
    body.insert("model".to_string(), Value::from(api_model));
    body.insert("stream".to_string(), Value::from(true));
    // §4.4: without this the provider sends no usage chunk and §4.8 falls
    // back to estimates for every call.
    body.insert(
        "stream_options".to_string(),
        serde_json::json!({"include_usage": true}),
    );
    if max_completion {
        body.insert(
            "max_completion_tokens".to_string(),
            Value::from(req.max_tokens),
        );
    } else {
        body.insert("max_tokens".to_string(), Value::from(req.max_tokens));
    }
    if let Some(temperature) = req.temperature {
        body.insert("temperature".to_string(), Value::from(temperature));
    }
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req.tools.iter().map(openai_tool).collect();
        body.insert("tools".to_string(), Value::Array(tools));
        body.insert("tool_choice".to_string(), Value::from("auto"));
    }
    if let Some(format) = &req.response_format {
        body.insert("response_format".to_string(), format.clone());
    }
    body.insert(
        "messages".to_string(),
        Value::Array(openai_messages(&req.messages)),
    );
    Value::Object(body)
}

/// One Ollama message: roles and text as `OpenAI`, images as `images: [b64]`,
/// tool calls whole (Ollama takes the object, not a string).
fn ollama_message(message: &Message) -> Value {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut text = String::new();
    let mut images: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for block in &message.blocks {
        match block {
            Block::Text { text: part } | Block::ThinkingPlaceholder { text: part } => {
                text.push_str(part);
            }
            Block::Image { data_b64, .. } => images.push(Value::from(data_b64.clone())),
            Block::Reasoning { .. } => {}
            Block::ToolCall { name, input, .. } => tool_calls.push(serde_json::json!({
                "function": {"name": name, "arguments": input},
            })),
            Block::ToolResult { content, .. } => {
                for inner in content {
                    if let Block::Text { text: part }
                    | Block::Reasoning { text: part, .. }
                    | Block::ThinkingPlaceholder { text: part } = inner
                    {
                        text.push_str(part);
                    }
                }
            }
        }
    }
    let mut message = Map::new();
    message.insert("role".to_string(), Value::from(role));
    message.insert("content".to_string(), Value::from(text));
    if !images.is_empty() {
        message.insert("images".to_string(), Value::Array(images));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    Value::Object(message)
}

fn ollama_body(api_model: &str, req: &ModelRequest) -> Value {
    let mut body = Map::new();
    body.insert("model".to_string(), Value::from(api_model));
    body.insert("stream".to_string(), Value::from(true));
    let messages: Vec<Value> = req.messages.iter().map(ollama_message).collect();
    body.insert("messages".to_string(), Value::Array(messages));
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req.tools.iter().map(openai_tool).collect();
        body.insert("tools".to_string(), Value::Array(tools));
    }
    let mut options = Map::new();
    options.insert("num_predict".to_string(), Value::from(req.max_tokens));
    if let Some(temperature) = req.temperature {
        options.insert("temperature".to_string(), Value::from(temperature));
    }
    body.insert("options".to_string(), Value::Object(options));
    Value::Object(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::registry::bundled;

    fn fixture_request() -> ModelRequest {
        let system = Message::new(
            Role::System,
            vec![Block::Text {
                text: "Be brief.".to_string(),
            }],
            1,
        );
        let user = Message::new(
            Role::User,
            vec![Block::Text {
                text: "Hi".to_string(),
            }],
            1,
        );
        let assistant = Message::new(
            Role::Assistant,
            vec![
                Block::Text {
                    text: "Calling.".to_string(),
                },
                Block::ToolCall {
                    call_id: "c1".to_string(),
                    name: "get_weather".to_string(),
                    input: serde_json::json!({"city": "Paris"}),
                    partial: false,
                    parse_error: None,
                },
            ],
            1,
        );
        let tool = Message::new(
            Role::Tool,
            vec![Block::ToolResult {
                call_id: "c1".to_string(),
                content: vec![Block::Text {
                    text: "sunny".to_string(),
                }],
                is_error: false,
            }],
            1,
        );
        ModelRequest {
            model: "test/m".to_string(),
            messages: vec![system, user, assistant, tool],
            tools: vec![ToolSpec {
                name: "get_weather".to_string(),
                description: "Weather.".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            max_tokens: 100,
            temperature: Some(0.5),
            response_format: None,
            turn_id: 7,
        }
    }

    /// T-PROV-003's shape, per adapter: `capabilities()` equals the §4.9 row,
    /// and `id()` is the registry provider key.
    #[test]
    fn capabilities_equal_the_registry_row() {
        let registry = bundled();
        let adapter = AnthropicAdapter::new(
            "anthropic/claude-sonnet-4-5",
            registry,
            Some("key".to_string()),
            None,
            None,
        );
        assert_eq!(adapter.id().as_str(), "anthropic");
        let (_, entry) = registry
            .resolve("anthropic/claude-sonnet-4-5")
            .expect("bundled");
        assert_eq!(adapter.capabilities(), Capabilities::from_entry(entry));

        let adapter = OllamaAdapter::new("ollama/qwen2.5-coder:14b", registry, None, None, None);
        assert_eq!(adapter.id().as_str(), "ollama");
        let (_, entry) = registry
            .resolve("ollama/qwen2.5-coder:14b")
            .expect("bundled");
        assert_eq!(adapter.capabilities(), Capabilities::from_entry(entry));
    }

    /// `health()` is the whole pre-flight, with no network: unknown model,
    /// bad URL, missing key, and the ready state — plus Ollama's keyless
    /// exception (§4.10).
    #[test]
    fn health_reports_state_without_touching_the_network() {
        let registry = bundled();
        let adapter =
            AnthropicAdapter::new("no/such-model", registry, Some("k".into()), None, None);
        assert_eq!(adapter.health(), ProviderHealth::UnknownModel);

        let adapter =
            AnthropicAdapter::new("anthropic/claude-sonnet-4-5", registry, None, None, None);
        assert_eq!(adapter.health(), ProviderHealth::NoCredentials);

        let adapter = AnthropicAdapter::new(
            "anthropic/claude-sonnet-4-5",
            registry,
            Some("k".into()),
            None,
            Some("http://[::1:broken".to_string()),
        );
        assert!(
            matches!(adapter.health(), ProviderHealth::Misconfigured { .. }),
            "an unparsable base_url is Misconfigured"
        );

        let adapter = AnthropicAdapter::new(
            "anthropic/claude-sonnet-4-5",
            registry,
            Some("k".into()),
            None,
            None,
        );
        assert_eq!(adapter.health(), ProviderHealth::Ready);

        let adapter = OllamaAdapter::new("ollama/qwen2.5-coder:14b", registry, None, None, None);
        assert_eq!(adapter.health(), ProviderHealth::Ready);

        let declared = Capabilities::baseline();
        let adapter =
            OpenaiCompatibleAdapter::new("my-model", "not a url".to_string(), None, declared, None);
        assert!(
            matches!(adapter.health(), ProviderHealth::Misconfigured { .. }),
            "a proxy without a URL is Misconfigured, not Ready"
        );
    }

    /// `stream()` refuses before any socket: the health states become their
    /// §4.5 faults, and none of them is retryable-by-accident.
    #[tokio::test]
    async fn stream_refuses_before_any_socket() {
        let registry = bundled();
        let adapter =
            AnthropicAdapter::new("no/such-model", registry, Some("k".into()), None, None);
        let Err(err) = adapter
            .stream(fixture_request(), CancellationToken::new())
            .await
        else {
            panic!("unknown model faults")
        };
        assert_eq!(err.fault, ProviderFault::NoModel);

        let adapter = OpenaiAdapter::new("openai/gpt-5.1-codex", registry, None, None, None);
        let Err(err) = adapter
            .stream(fixture_request(), CancellationToken::new())
            .await
        else {
            panic!("missing key faults")
        };
        assert_eq!(err.fault, ProviderFault::Auth);
    }

    /// §4.10's env steps: `CAIRN_<PROVIDER>_API_KEY` wins, the registry's
    /// standard name is the fallback, and empty is absent. Unique variable
    /// names keep this safe under the harness's parallelism.
    #[test]
    fn env_lookup_prefers_the_cairn_variable() {
        std::env::set_var("CAIRN_TPROV_API_KEY", "first");
        std::env::set_var("TPROV_STANDARD_KEY", "second");
        assert_eq!(
            env_key("tprov", Some("TPROV_STANDARD_KEY")),
            Some("first".to_string())
        );
        std::env::remove_var("CAIRN_TPROV_API_KEY");
        assert_eq!(
            env_key("tprov", Some("TPROV_STANDARD_KEY")),
            Some("second".to_string())
        );
        std::env::set_var("CAIRN_TPROV_API_KEY", "");
        assert_eq!(
            env_key("tprov", Some("TPROV_STANDARD_KEY")),
            Some("second".to_string()),
            "empty is absent"
        );
        std::env::remove_var("CAIRN_TPROV_API_KEY");
        std::env::remove_var("TPROV_STANDARD_KEY");
        assert_eq!(env_key("tprov", Some("TPROV_STANDARD_KEY")), None);
        assert_eq!(
            env_key_name("openai_compatible"),
            "CAIRN_OPENAI_COMPATIBLE_API_KEY"
        );
    }

    /// Only the o-series reasoning models take `max_completion_tokens`
    /// (§4.4): a name rule, spelled out and pinned.
    #[test]
    fn only_o_series_models_want_max_completion_tokens() {
        assert!(wants_max_completion("openai/o4-mini"));
        assert!(wants_max_completion("openai/o1"));
        assert!(!wants_max_completion("openai/gpt-5.1-codex"));
        assert!(!wants_max_completion("anthropic/claude-sonnet-4-5"));
        assert!(!wants_max_completion("bare-name"));
    }

    /// §4.4's Anthropic column: provider-side model name, system parameter
    /// with one breakpoint, the four message shapes, tools with auto choice.
    #[test]
    fn anthropic_shaping_matches_the_column() {
        let body = shape_body(
            Shape::Anthropic,
            "anthropic/claude-sonnet-4-5",
            true,
            &fixture_request(),
        );
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["stream"], true);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["system"][0]["text"], "Be brief.");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        let roles: Vec<&str> = body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .map(|message| message["role"].as_str().expect("role"))
            .collect();
        assert_eq!(roles, vec!["user", "assistant", "user"]);
        assert_eq!(body["messages"][1]["content"][1]["type"], "tool_use");
        assert_eq!(body["messages"][1]["content"][1]["id"], "c1");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "c1");
        assert_eq!(body["tools"][0]["name"], "get_weather");
        assert_eq!(body["tool_choice"]["type"], "auto");
    }

    /// §4.4's `OpenAI` column: `include_usage` always on, the token-limit
    /// spelling per model, `auto` choice, and the fanned-out `tool` message.
    #[test]
    fn openai_shaping_matches_the_column() {
        let body = shape_body(
            Shape::Openai,
            "openai/gpt-5.1-codex",
            true,
            &fixture_request(),
        );
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["max_tokens"], 100);
        assert!(body.get("max_completion_tokens").is_none());
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"][0]["function"]["name"], "get_weather");
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 4, "system, user, assistant, tool");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "c1");
        assert_eq!(messages[2]["content"], "Calling.");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "c1");
        assert_eq!(messages[3]["content"], "sunny");

        let body = shape_body(Shape::Openai, "openai/o4-mini", true, &fixture_request());
        assert_eq!(body["max_completion_tokens"], 100);
        assert!(body.get("max_tokens").is_none());
    }

    /// §4.4's Ollama column: `num_predict` options, whole-object tool
    /// arguments, `images` beside `content`.
    #[test]
    fn ollama_shaping_matches_the_column() {
        let mut req = fixture_request();
        req.messages[1].blocks.push(Block::Image {
            media_type: MediaType::Png,
            data_b64: "aGk=".to_string(),
            alt: None,
        });
        let body = shape_body(Shape::Ollama, "ollama/qwen2.5-coder:14b", true, &req);
        assert_eq!(body["model"], "qwen2.5-coder:14b");
        assert_eq!(body["stream"], true);
        assert_eq!(body["options"]["num_predict"], 100);
        assert_eq!(body["options"]["temperature"], 0.5);
        assert_eq!(body["messages"][1]["images"][0], "aGk=");
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["name"],
            "get_weather"
        );
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["arguments"]["city"], "Paris",
            "Ollama takes the object, not a string"
        );
    }

    /// `count_tokens` is the §4.8 estimator, not a request.
    #[tokio::test]
    async fn count_tokens_estimates_locally() {
        let registry = bundled();
        let adapter =
            AnthropicAdapter::new("anthropic/claude-sonnet-4-5", registry, None, None, None);
        let count = adapter
            .count_tokens(&fixture_request())
            .await
            .expect("estimates");
        assert!(count.estimated, "REQ-PROV-011's flag");
        assert!(count.input > 0);
    }
    /// T-PROV-004's shaping half: a model with `tool_calling == false` sends
    /// no native `tools` — the definitions move into the system prompt as
    /// §4.6's section instead. (Extraction and injection are `fallback`'s;
    /// the per-turn counting is the turn loop's.)
    #[test]
    fn fallback_models_shape_tools_into_the_system_prompt() {
        let body = shape_body(Shape::Openai, "some/proxy-model", false, &fixture_request());
        assert!(body.get("tools").is_none(), "no native parameter");
        assert!(body.get("tool_choice").is_none());
        let sectioned = body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter(|message| message["role"] == "system")
            .any(|message| {
                message["content"]
                    .as_str()
                    .unwrap_or("")
                    .contains("Available tools:")
            });
        assert!(sectioned, "the section is a system message");

        let body = shape_body(Shape::Anthropic, "some/claude", false, &fixture_request());
        assert!(body.get("tools").is_none());
        assert!(
            body["system"]
                .as_array()
                .expect("system")
                .iter()
                .any(|block| block["text"].as_str().unwrap_or("").contains("<tool>")),
            "the section is the system parameter"
        );
    }
}
