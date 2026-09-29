#![allow(unused_imports)]

use tabit_engine::tool::ToolContext;
use tabit_derive::rig_tool;

#[rig_tool]
fn multiple_contexts(
    #[rig(context)]
    first: &mut ToolContext,
    #[rig(context)]
    second: &mut ToolContext,
) -> Result<(), tabit_providers::tool::ToolExecutionError> {
    let _ = (first, second);
    Ok(())
}

fn main() {}
