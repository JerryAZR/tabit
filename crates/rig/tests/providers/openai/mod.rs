mod support;

mod cassette {
    mod agent;
    mod chat_history;
    mod completions_api;
    mod document_ordering;
    mod gpt_5_6_reasoning;
    mod models;
    mod openai_compatible_reasoning_content;
    mod permission_control;
    mod reasoning_roundtrip;
    mod reasoning_tool_roundtrip;
    mod request_hook;
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
    mod url_pdf_document;
    mod vllm;
}
