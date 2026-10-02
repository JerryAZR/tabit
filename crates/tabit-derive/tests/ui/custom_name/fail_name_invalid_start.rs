#![allow(dead_code)]

use tabit_derive::rig_tool;

#[rig_tool(name = "9bad")]
fn invalid_name_start() -> Result<String, tabit_providers::tool::ToolExecutionError> {
    Ok("ok".to_string())
}

fn main() {}
