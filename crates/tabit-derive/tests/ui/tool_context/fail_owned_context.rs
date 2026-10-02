use tabit_derive::rig_tool;

#[rig_tool]
fn owned_context(
    context: tabit_engine::tool::ToolContext,
) -> Result<(), tabit_providers::tool::ToolExecutionError> {
    let _ = context;
    Ok(())
}

fn main() {}
