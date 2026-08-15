//! Conversation memory helpers for deterministic agent tests.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use crate::{
    completion::Message,
    memory::{ConversationMemory, InMemoryConversationMemory, MemoryError},
    wasm_compat::WasmBoxedFuture,
};

/// Memory backend that records load and append calls while delegating storage to
/// [`InMemoryConversationMemory`].
#[derive(Clone, Default)]
pub struct CountingMemory {
    inner: InMemoryConversationMemory,
    loads: Arc<AtomicUsize>,
    appends: Arc<AtomicUsize>,
}

impl CountingMemory {
    /// Return the backing in-memory store.
    pub fn inner(&self) -> &InMemoryConversationMemory {
        &self.inner
    }

    /// Return the number of calls to [`ConversationMemory::load`].
    pub fn load_count(&self) -> usize {
        self.loads.load(Ordering::SeqCst)
    }

    /// Return the number of calls to [`ConversationMemory::append`].
    pub fn append_count(&self) -> usize {
        self.appends.load(Ordering::SeqCst)
    }
}

impl ConversationMemory for CountingMemory {
    fn load<'a>(
        &'a self,
        conversation_id: &'a str,
    ) -> WasmBoxedFuture<'a, Result<Vec<Message>, MemoryError>> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.inner.load(conversation_id)
    }

    fn append<'a>(
        &'a self,
        conversation_id: &'a str,
        messages: Vec<Message>,
    ) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
        self.appends.fetch_add(1, Ordering::SeqCst);
        self.inner.append(conversation_id, messages)
    }

    fn clear<'a>(
        &'a self,
        conversation_id: &'a str,
    ) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
        self.inner.clear(conversation_id)
    }
}

/// Memory backend that always fails on load and no-ops append and clear.
#[derive(Clone)]
pub struct FailingMemory {
    message: String,
}

impl FailingMemory {
    /// Create a load-failing memory backend.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Default for FailingMemory {
    fn default() -> Self {
        Self::new("load boom")
    }
}

impl ConversationMemory for FailingMemory {
    fn load<'a>(
        &'a self,
        _conversation_id: &'a str,
    ) -> WasmBoxedFuture<'a, Result<Vec<Message>, MemoryError>> {
        let message = self.message.clone();
        Box::pin(async move { Err(MemoryError::backend(std::io::Error::other(message))) })
    }

    fn append<'a>(
        &'a self,
        _conversation_id: &'a str,
        _messages: Vec<Message>,
    ) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
        Box::pin(async { Ok(()) })
    }

    fn clear<'a>(
        &'a self,
        _conversation_id: &'a str,
    ) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Memory backend that loads empty history and always fails on append.
#[derive(Clone)]
pub struct AppendFailingMemory {
    message: String,
}

impl AppendFailingMemory {
    /// Create an append-failing memory backend.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Default for AppendFailingMemory {
    fn default() -> Self {
        Self::new("append boom")
    }
}

impl ConversationMemory for AppendFailingMemory {
    fn load<'a>(
        &'a self,
        _conversation_id: &'a str,
    ) -> WasmBoxedFuture<'a, Result<Vec<Message>, MemoryError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn append<'a>(
        &'a self,
        _conversation_id: &'a str,
        _messages: Vec<Message>,
    ) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
        let message = self.message.clone();
        Box::pin(async move { Err(MemoryError::backend(std::io::Error::other(message))) })
    }

    fn clear<'a>(
        &'a self,
        _conversation_id: &'a str,
    ) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::{AppendFailingMemory, CountingMemory, FailingMemory};
    use crate::completion::Message;
    use crate::memory::ConversationMemory as _;

    #[tokio::test]
    async fn counting_memory_records_loads_appends_and_clears() {
        let memory = CountingMemory::default();

        let messages = memory.load("conv-1").await.expect("load should succeed");
        assert!(messages.is_empty());

        memory
            .append("conv-1", vec![Message::user("hello")])
            .await
            .expect("append should succeed");

        let messages = memory.load("conv-1").await.expect("load should succeed");
        assert_eq!(messages.len(), 1);

        memory.clear("conv-1").await.expect("clear should succeed");
        let messages = memory.load("conv-1").await.expect("load should succeed");
        assert!(messages.is_empty());

        assert_eq!(memory.load_count(), 3);
        assert_eq!(memory.append_count(), 1);
    }

    #[tokio::test]
    async fn failing_memory_fails_load_and_no_ops_append_and_clear() {
        let memory = FailingMemory::new("cannot reach store");

        let error = memory.load("conv-1").await.expect_err("load must fail");
        assert!(error.to_string().contains("cannot reach store"));

        memory
            .append("conv-1", vec![Message::user("hello")])
            .await
            .expect("append is a no-op success");
        memory
            .clear("conv-1")
            .await
            .expect("clear is a no-op success");
    }

    #[tokio::test]
    async fn append_failing_memory_loads_empty_and_fails_append() {
        let memory = AppendFailingMemory::default();

        let messages = memory.load("conv-1").await.expect("load should succeed");
        assert!(messages.is_empty());

        let error = memory
            .append("conv-1", vec![Message::user("hello")])
            .await
            .expect_err("append must fail");
        assert!(error.to_string().contains("append boom"));

        memory
            .clear("conv-1")
            .await
            .expect("clear is a no-op success");
    }
}
