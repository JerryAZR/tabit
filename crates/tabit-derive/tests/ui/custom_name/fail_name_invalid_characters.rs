#![allow(dead_code)]

use tabit_derive::rig_tool;

#[rig_tool(name = "bad name!")]
fn invalid_name_characters() -> Result<String, tabit_providers::tool::ToolExecutionError> {
    Ok("ok".to_string())
}

fn main() {}
