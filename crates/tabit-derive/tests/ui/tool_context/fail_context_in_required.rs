#![allow(unused_imports)]

use tabit_engine::tool::ToolContext;
use tabit_derive::rig_tool;

#[rig_tool(required(context))]
fn context_in_required(
    #[rig(context)]
    context: &mut ToolContext,
    query: String,
) -> Result<String, tabit_providers::tool::ToolExecutionError> {
    let _ = context;
    Ok(query)
}

fn main() {}
