mod support;

mod regressions;

mod cassette {
    mod agent;
    mod chat_history;
    mod completions_api;
    mod document_ordering;
    mod extractor;
    mod extractor_usage;
    mod gpt_5_6_reasoning;
    mod models;
    mod multi_extract;
    mod openai_compatible_reasoning_content;
    mod permission_control;
    mod reasoning_roundtrip;
    mod reasoning_tool_roundtrip;
    mod request_hook;
    mod response_retry;
    mod response_schema;
    mod responses_behaviors;
    mod responses_input_item;
    mod responses_sessions;
    mod responses_tool_args;
    mod responses_tool_choice;
    mod streaming;
    mod streaming_grammar;
    mod streaming_grammar_chat;
    mod streaming_tools;
    mod structured_output;
    mod typed_prompt_tools;
    mod url_pdf_document;
    mod vllm;
}

// Live (network-only) OpenAI coverage is trimmed in this vendored facade:
// upstream's `tests/providers/openai/live/*` modules are `#[ignore]` tests that
// require a real `OPENAI_API_KEY` and network access. They are dropped to keep
// the offline replay suite self-contained (see VENDOR.md). The cassette/
// modules above cover the replayed surface.
mod live {}
