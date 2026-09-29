#![allow(dead_code)]

use tabit_derive::rig_tool;

#[rig_tool(name = 123)]
fn invalid_name_value() -> Result<String, tabit_providers::tool::ToolExecutionError> {
    Ok("ok".to_string())
}

fn main() {}
