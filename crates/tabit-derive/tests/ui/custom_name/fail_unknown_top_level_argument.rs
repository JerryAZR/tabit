#![allow(dead_code)]

use tabit_derive::rig_tool;

#[rig_tool(nam = "search-docs")]
fn unknown_argument() -> Result<String, tabit_providers::tool::ToolExecutionError> {
    Ok("ok".to_string())
}

fn main() {}
