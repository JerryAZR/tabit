# Delete the Chat trait's last consumers: one-shot smokes -> plain .prompt;
# history-carrying sites -> the cell door; the chained document test uses a
# local one-shot SteeringSource (the session's pattern).
import io

def load(p):
    return io.open(p, encoding="utf-8").read()

def save(p, t):
    io.open(p, "w", encoding="utf-8", newline="").write(t)

def swap(t, old, new, count=1, p=""):
    got = t.count(old)
    assert got == count, f"{p}: x{got} (want {count}): {old[:60]!r}"
    return t.replace(old, new)

# ---- one-shot smokes ----
for p, prompt, msg in [
    ("crates/rig/tests/providers/anthropic/cassette/opus_4_7.rs",
     "reasoning::TOOL_USER_PROMPT",
     '"adaptive thinking tool chat should succeed"'),
    ("crates/rig/tests/providers/anthropic/cassette/reasoning_tool_roundtrip.rs",
     "reasoning::TOOL_USER_PROMPT",
     '"[anthropic] Non-streaming chat failed - likely 400 from dropped reasoning",'),
    ("crates/rig/tests/providers/openai/cassette/reasoning_tool_roundtrip.rs",
     "reasoning::TOOL_USER_PROMPT",
     '"[openai] Non-streaming chat failed - likely 400 from dropped reasoning"'),
    ("crates/rig/tests/providers/openai/cassette/openai_compatible_reasoning_content.rs",
     "reasoning::TOOL_USER_PROMPT",
     '"OpenAI-compatible provider should accept replayed reasoning content"'),
]:
    t = load(p)
    old = f"""            let result = agent
                .chat({prompt}, &mut Vec::<Message>::new())
                .await
                .expect({msg});"""
    new = f"""            let result = agent
                .prompt({prompt})
                .await
                .expect({msg});"""
    t = swap(t, old, new, p=p)
    save(p, t)

t = load("crates/rig/tests/providers/openai/cassette/openai_compatible_reasoning_content.rs")
t = swap(t, "use rig::completion::{Chat, Message};", "use rig::completion::Message;")
save("crates/rig/tests/providers/openai/cassette/openai_compatible_reasoning_content.rs", t)

# ---- responses_behaviors: seeded history -> seeded cell ----
p = "crates/rig/tests/providers/openai/cassette/responses_behaviors.rs"
t = load(p)
t = swap(t,
    """            let mut history = vec![
                Message::user("Hello!"),
                Message::assistant("Hi! How can I help you today?"),
                Message::system(
                    "The user's codename is FALCON-9. Always refer to the user by codename.",
                ),
            ];

            let result = agent
                .chat("What is my codename?", &mut history)
                .await
                .expect("chat with a mid-conversation system message should succeed");""",
    """            let cell = std::sync::Arc::new(std::sync::RwLock::new(
                tabit_log::ContextManager::seeded(vec![
                    Message::user("Hello!"),
                    Message::assistant("Hi! How can I help you today?"),
                    Message::system(
                        "The user's codename is FALCON-9. Always refer to the user by codename.",
                    ),
                    Message::user("What is my codename?"),
                ]),
            ));

            let result = agent
                .prompt_over(cell)
                .await
                .expect("chat with a mid-conversation system message should succeed");""",
    p=p)
save(p, t)

# ---- document_file_id: empty cell + per-run one-shot steers ----
p = "crates/rig/tests/providers/anthropic/cassette/document_file_id.rs"
t = load(p)
t = swap(t,
    """                let mut history = Vec::new();

                let direct_message = direct_file_id_document_question(&file_id, 2);""",
    """                let cell = std::sync::Arc::new(std::sync::RwLock::new(
                    tabit_log::ContextManager::seeded(Vec::new()),
                ));

                let direct_message = direct_file_id_document_question(&file_id, 2);""",
    p=p)
t = swap(t,
    """                let response = agent
                    .chat(provider_native_roundtrip_message, &mut history)
                    .await
                    .expect("Messages API should read uploaded PDF by file_id");
                assert_verifier_response(&response, PAGE_TWO_VERIFIER);
                assert_history_preserves_single_file_id(&history, &file_id);

                let follow_up = agent
                    .chat(
                        "Using the same PDF from the conversation history, what verifier token is printed on page 3? Reply with only the exact token.",
                        &mut history,
                    )
                    .await
                    .expect("Messages API should reuse file_id document from chat history");
                assert_verifier_response(&follow_up, PAGE_THREE_VERIFIER);
                assert_history_preserves_single_file_id(&history, &file_id);""",
    """                let response = agent
                    .prompt_over(cell.clone())
                    .steering(std::sync::Arc::new(OneShotSteer::new(
                        provider_native_roundtrip_message,
                    )))
                    .await
                    .expect("Messages API should read uploaded PDF by file_id");
                assert_verifier_response(&response, PAGE_TWO_VERIFIER);
                assert_history_preserves_single_file_id(
                    &tabit_log::lock::read(&cell).messages(),
                    &file_id,
                );

                let follow_up = agent
                    .prompt_over(cell.clone())
                    .steering(std::sync::Arc::new(OneShotSteer::new(
                        rig::completion::Message::user(
                            "Using the same PDF from the conversation history, what verifier token is printed on page 3? Reply with only the exact token.",
                        ),
                    )))
                    .await
                    .expect("Messages API should reuse file_id document from chat history");
                assert_verifier_response(&follow_up, PAGE_THREE_VERIFIER);
                assert_history_preserves_single_file_id(
                    &tabit_log::lock::read(&cell).messages(),
                    &file_id,
                );""",
    p=p)
t = swap(t, "use rig::completion::{Chat, Prompt};", "use rig::completion::Prompt;", p=p)

helper = '''/// One steering message, delivered once at the run's first convergence —
/// the session's pattern for "answer this over my conversation", local
/// to this test.
struct OneShotSteer(std::sync::Mutex<Option<rig::completion::Message>>);

impl OneShotSteer {
    fn new(message: rig::completion::Message) -> Self {
        Self(std::sync::Mutex::new(Some(message)))
    }
}

impl rig_agent::SteeringSource for OneShotSteer {
    fn drain(&self) -> Vec<(String, rig::completion::Message)> {
        self.0
            .lock()
            .expect("steer lock")
            .take()
            .map(|message| vec![(rig::id::generate(), message)])
            .unwrap_or_default()
    }
}

#[tokio::test]'''
first = t.index("#[tokio::test]")
t = t[:first] + helper + t[first + len("#[tokio::test]"):]
assert t.count("struct OneShotSteer") == 1
save(p, t)
print("chat consumers migrated")
