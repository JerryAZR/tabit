//! Test utilities for the classic runtime and its provider-facing acceptance tests.

mod model_conformance;
mod tools;

/// A [`HookContext`](crate::agent::HookContext) over a caller-built
/// capability map — the host-side seam for driving mounted hooks
/// directly. The engine builds its own context per run; a host
/// testing the policy hooks it mounts (a permission gate, an
/// auditor) drives them the same way, over the same capability
/// lookup.
pub fn hook_context(capabilities: crate::tool::ToolContext) -> crate::agent::HookContext {
    crate::agent::HookContext::new(capabilities)
}

pub use model_conformance::{
    ConformanceToolError, ScenarioError, ScenarioReport, buffered_streaming_text_parity,
    cancellation_and_max_turns, complex_tool_arguments, decode_structured_output, hook_rewrites,
    invalid_tool_recovery, optional_argument, parallel_tools, sequential_tools, streaming_tool,
    tool_choice_modes, tool_output_serialization, validate_cancelled_failure,
    validate_extraction_fields, validate_max_turns_failure, validate_protocol_hygiene,
    validate_result_redaction, validate_rewritten_arguments, zero_argument_tool,
};
pub use rig_core::test_utils::*;
pub use tools::{
    MockAddTool, MockBarrierTool, MockContextProbeTool, MockControlledTool, MockDeniedTool,
    MockExampleTool, MockFailingTool, MockFailure, MockHandledFailureTool, MockImageOutputTool,
    MockMetadataTool, MockObjectOutputTool, MockOperationArgs, MockRequestId, MockStringOutputTool,
    MockSubtractTool, MockToolError, SessionId, mock_math_toolset,
};
