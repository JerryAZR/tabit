#![allow(dead_code)]

use tabit_derive::rig_tool;

#[rig_tool(name = "search-docs")]
fn search_docs_impl() -> Result<String, tabit_providers::tool::ToolExecutionError> {
    Ok("ok".to_string())
}

fn main() {}
