#![allow(dead_code)]

use tabit_derive::rig_tool;

#[rig_tool(name = "a2345678a2345678a2345678a2345678a2345678a2345678a2345678a2345678x")]
fn invalid_name_length() -> Result<String, tabit_providers::tool::ToolExecutionError> {
    Ok("ok".to_string())
}

fn main() {}
