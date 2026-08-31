//! Context-free tool authoring contracts.
//!
//! Portable tools receive owned, deserialized arguments only. Runtime identity,
//! authorization, mutable context, capability state, and lifecycle metadata
//! remain outside this module.

use std::sync::Arc;

use serde::Deserialize;

use crate::{
    completion::ToolDefinition,
    wasm_compat::{WasmBoxedFuture, WasmCompatSend, WasmCompatSync},
};

use super::{IntoToolOutput, ToolExecutionError, ToolOutput};

/// A context-free typed tool that can be executed by any Rig runtime.
pub trait PortableTool: Sized + WasmCompatSend + WasmCompatSync {
    /// Unique registration and provider-facing name.
    const NAME: &'static str;
    /// Owned JSON arguments.
    type Args: for<'de> Deserialize<'de> + WasmCompatSend + WasmCompatSync;
    /// Canonical model-visible output.
    type Output: IntoToolOutput + WasmCompatSend;
    /// Concrete author-facing failure.
    type Error: std::error::Error + WasmCompatSend + WasmCompatSync + 'static;

    /// Model-facing description.
    fn description(&self) -> String;

    /// JSON Schema for arguments.
    fn parameters(&self) -> serde_json::Value;

    /// Normalize a concrete failure at the runtime effect boundary.
    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::from_error(error)
    }

    /// Execute one owned invocation without runtime access.
    fn call(
        &self,
        arguments: Self::Args,
    ) -> impl Future<Output = Result<Self::Output, Self::Error>> + WasmCompatSend;
}

trait PortableDynamicCallback:
    Fn(serde_json::Value) -> WasmBoxedFuture<'static, Result<ToolOutput, ToolExecutionError>>
    + WasmCompatSend
    + WasmCompatSync
{
}

impl<F> PortableDynamicCallback for F where
    F: Fn(serde_json::Value) -> WasmBoxedFuture<'static, Result<ToolOutput, ToolExecutionError>>
        + WasmCompatSend
        + WasmCompatSync
{
}

/// A runtime-authored context-free tool implementation.
#[derive(Clone)]
pub struct PortableDynamicTool {
    name: String,
    description: String,
    parameters: serde_json::Value,
    callback: Arc<dyn PortableDynamicCallback>,
}

impl std::fmt::Debug for PortableDynamicTool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PortableDynamicTool")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("parameters", &self.parameters)
            .finish_non_exhaustive()
    }
}

impl PortableDynamicTool {
    /// Create a context-free dynamic tool from an owned async callback.
    pub fn new<F>(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
        callback: F,
    ) -> Self
    where
        F: Fn(
                serde_json::Value,
            ) -> WasmBoxedFuture<'static, Result<ToolOutput, ToolExecutionError>>
            + WasmCompatSend
            + WasmCompatSync
            + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            callback: Arc::new(callback),
        }
    }

    /// Provider-facing name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Provider-facing definition.
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    /// Execute the callback with owned arguments.
    pub async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> Result<ToolOutput, ToolExecutionError> {
        (self.callback)(arguments).await
    }
}

/// Generate provider-facing metadata for a portable typed tool.
pub fn portable_tool_definition<T>(tool: &T) -> ToolDefinition
where
    T: PortableTool,
{
    ToolDefinition {
        name: T::NAME.to_owned(),
        description: tool.description(),
        parameters: tool.parameters(),
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Deserialize)]
    struct AddArgs {
        left: i64,
        right: i64,
    }

    #[derive(Serialize)]
    struct Sum {
        value: i64,
    }

    struct Add;

    impl PortableTool for Add {
        const NAME: &'static str = "add";
        type Args = AddArgs;
        type Output = Sum;
        type Error = Infallible;

        fn description(&self) -> String {
            "Add two integers".to_string()
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn call(&self, arguments: Self::Args) -> Result<Self::Output, Self::Error> {
            Ok(Sum {
                value: arguments.left + arguments.right,
            })
        }
    }

    #[tokio::test]
    async fn portable_tools_execute_without_runtime_context() {
        let output = Add.call(AddArgs { left: 2, right: 3 }).await;
        let Ok(output) = output;
        assert_eq!(output.value, 5);
        assert_eq!(portable_tool_definition(&Add).name, "add");
    }

    #[tokio::test]
    async fn portable_dynamic_tools_receive_owned_arguments() {
        let tool = PortableDynamicTool::new(
            "echo",
            "Echo a JSON value",
            serde_json::json!({"type": "object"}),
            |arguments| Box::pin(async move { Ok(ToolOutput::json(arguments)) }),
        );

        assert_eq!(tool.name(), "echo");
        assert_eq!(tool.definition().name, "echo");
        assert_eq!(tool.definition().description, "Echo a JSON value");

        let arguments = serde_json::json!({"value": "hello"});
        let output = tool
            .execute(arguments.clone())
            .await
            .expect("the echo callback always succeeds");
        assert_eq!(output.as_json(), Some(&arguments));
    }

    #[tokio::test]
    async fn dynamic_tool_debug_reports_metadata_without_the_callback() {
        let tool = PortableDynamicTool::new(
            "echo",
            "Echo a JSON value",
            serde_json::json!({"type": "object"}),
            |_arguments| Box::pin(async move { Ok(ToolOutput::text("ok")) }),
        );

        let debug = format!("{tool:?}");
        assert!(debug.contains("echo"));
        assert!(debug.contains("Echo a JSON value"));
        assert!(debug.contains("type"));

        // The debug test's callback is a real callback: it must still execute.
        let output = tool.execute(serde_json::json!({})).await.unwrap();
        assert_eq!(output.as_text(), Some("ok"));
    }

    #[derive(Debug, thiserror::Error)]
    #[error("kaboom")]
    struct Kaboom;

    struct Failing;

    impl PortableTool for Failing {
        const NAME: &'static str = "failing";

        type Args = ();
        type Output = ();
        type Error = Kaboom;

        fn description(&self) -> String {
            "Always fails".to_string()
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }

        async fn call(&self, _arguments: Self::Args) -> Result<Self::Output, Self::Error> {
            Err(Kaboom)
        }
    }

    #[tokio::test]
    async fn map_error_normalizes_a_concrete_failure_at_the_boundary() {
        assert_eq!(Failing.description(), "Always fails");
        assert_eq!(Failing.parameters(), serde_json::json!({}));
        assert!(Failing.call(()).await.is_err());

        let error = Failing.map_error(Kaboom);

        assert_eq!(error.kind(), crate::tool::ToolErrorKind::Other);
        assert_eq!(error.message(), "kaboom");
        // Arbitrary sources are treated as operator-only diagnostics.
        assert_eq!(error.model_feedback(), Some("the tool failed"));
    }
}
