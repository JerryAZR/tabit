#![allow(dead_code)]

use tabit_derive::rig_tool;

#[rig_tool]
fn fallback_name_tool() -> Result<String, tabit_providers::tool::ToolExecutionError> {
    Ok("fallback".to_string())
}

fn main() {}
