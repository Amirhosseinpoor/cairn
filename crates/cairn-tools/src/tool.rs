//! The `Tool` trait (SPEC §3.4).

use std::time::Duration;

use cairn_core::cancel::CancellationToken;
use futures::future::BoxFuture;
use serde_json::Value;

use crate::types::{
    Idempotency, PathArg, PermissionClass, RequestInfo, ShellRequest, SideEffect, ToolContext,
    ToolError, ToolOutput,
};

/// One capability the model can call.
///
/// `execute` returns a boxed future because the trait is used as
/// `Arc<dyn Tool>` (§3.4 note: `async fn` in a trait is not object-safe).
pub trait Tool: Send + Sync + 'static {
    /// The name the model calls it by.
    fn name(&self) -> &'static str;
    /// Model-facing, at most 2,000 characters.
    fn description(&self) -> &'static str;
    /// JSON Schema draft 2020-12 for the input (§6.2).
    fn input_schema(&self) -> Value;
    /// JSON Schema for the `data` object of a successful result.
    fn output_schema(&self) -> Value;
    fn permission_class(&self) -> PermissionClass;
    fn side_effect(&self) -> SideEffect;
    fn idempotency(&self) -> Idempotency;
    fn timeout(&self) -> Duration;
    fn max_output_bytes(&self) -> u32;
    /// Takes the global serial lock (§6.6).
    fn requires_serial(&self) -> bool {
        false
    }

    /// The path-valued fields of `input`, so the pipeline can run the
    /// boundary checks (§6.5 steps 3–5) before the tool does anything.
    fn path_args(&self, _input: &Value) -> Vec<PathArg> {
        Vec::new()
    }

    /// The command and URL the permission engine should see.
    fn request_info(&self, _input: &Value) -> RequestInfo {
        RequestInfo::default()
    }

    /// A shell command line to analyse (§9.3) before permission is decided;
    /// its leaves, not the whole string, are what the rules see.
    fn shell_request(&self, _input: &Value) -> Option<ShellRequest> {
        None
    }

    fn execute(
        &self,
        input: Value,
        ctx: ToolContext,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>>;
}
