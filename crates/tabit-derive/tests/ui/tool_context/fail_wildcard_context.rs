#![allow(unused_imports)]

use tabit_engine::tool::ToolContext;
use tabit_derive::rig_tool;

// The runtime context parameter must be a nameable binding; a wildcard `_`
// is rejected — name it `_context` instead.
#[rig_tool]
fn wildcard_context(
    #[rig(context)] _: &mut ToolContext,
    value: String,
) -> Result<String, tabit_providers::tool::ToolExecutionError> {
    Ok(value)
}

fn main() {}
