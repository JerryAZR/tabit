#![allow(unused_imports)]

use tabit_engine::tool::ToolContext;
use tabit_derive::rig_tool;

#[rig_tool]
fn immutable_context(
    #[rig(context)] context: &ToolContext,
) -> Result<(), tabit_providers::tool::ToolExecutionError> {
    let _ = context;
    Ok(())
}

fn main() {}
