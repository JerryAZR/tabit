use tabit_derive::rig_tool;

// A non-string description used to be silently dropped; it must error.
#[rig_tool(params(limit = 3))]
fn search(limit: i64) -> Result<i64, tabit_providers::tool::ToolExecutionError> {
    Ok(limit)
}

fn main() {}
