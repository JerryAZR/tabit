//! The module defines the [EmbeddingsBuilder] struct which accumulates objects to be embedded
//! and batch generates the embeddings for each object when built.
//! Only types that implement the [Embed] trait can be added to the [EmbeddingsBuilder].

use std::{cmp::max, collections::HashMap};

use futures::{StreamExt, stream};

use crate::{
    OneOrMany,
    completion::Usage,
    embeddings::{
        Embed, EmbedError, Embedding, EmbeddingError, EmbeddingModel, EmbeddingResponse,
        embed::TextEmbedder,
    },
};

/// Builder for creating embeddings from one or more documents of type `T`.
/// Note: `T` can be any type that implements the [Embed] trait.
///
/// Using the builder is preferred over using [EmbeddingModel::embed_text] directly as
/// it will batch the documents in a single request to the model provider.
///
/// # Example
/// ```no_run
/// use rig_core::{
///     client::{EmbeddingsClient, ProviderClient},
///     embeddings::EmbeddingsBuilder,
///     providers::openai,
/// };
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// // Create OpenAI client
/// let openai_client = openai::Client::from_env()?;
///
/// let model = openai_client.embedding_model("text-embedding-3-small");
///
/// let embeddings = EmbeddingsBuilder::new(model.clone())
///     .documents(vec![
///         "1. *flurbo* (noun): A green alien that lives on cold planets.".to_string(),
///         "2. *flurbo* (noun): A fictional digital currency.".to_string(),
///         "1. *glarb-glarb* (noun): An ancient tool used by the ancestors of the inhabitants of planet Jiro to farm the land.".to_string(),
///         "2. *glarb-glarb* (noun): A fictional creature from marshlands.".to_string(),
///         "1. *linlingdong* (noun): A term used by inhabitants of the sombrero galaxy to describe humans.".to_string(),
///         "2. *linlingdong* (noun): A rare instrument.".to_string(),
///     ])?
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
#[non_exhaustive]
pub struct EmbeddingsBuilder<M, T>
where
    M: EmbeddingModel,
    T: Embed,
{
    model: M,
    documents: Vec<(T, Vec<String>)>,
}

impl<M, T> EmbeddingsBuilder<M, T>
where
    M: EmbeddingModel,
    T: Embed,
{
    /// Create a new embedding builder with the given embedding model
    pub fn new(model: M) -> Self {
        Self {
            model,
            documents: vec![],
        }
    }

    /// Add a document to be embedded to the builder. `document` must implement the [Embed] trait.
    pub fn document(mut self, document: T) -> Result<Self, EmbedError> {
        let mut embedder = TextEmbedder::default();
        document.embed(&mut embedder)?;

        self.documents.push((document, embedder.texts));

        Ok(self)
    }

    /// Add multiple documents to be embedded to the builder. `documents` must be iterable
    /// with items that implement the [Embed] trait.
    pub fn documents(self, documents: impl IntoIterator<Item = T>) -> Result<Self, EmbedError> {
        let builder = documents
            .into_iter()
            .try_fold(self, |builder, doc| builder.document(doc))?;

        Ok(builder)
    }
}

impl<M, T> EmbeddingsBuilder<M, T>
where
    M: EmbeddingModel,
    T: Embed + Send,
{
    /// Generate embeddings for all documents in the builder.
    ///
    /// Returns `(document, embeddings)` pairs. A document may produce one or many
    /// embeddings depending on how its [`Embed`] implementation uses [`TextEmbedder`].
    pub async fn build(self) -> Result<Vec<(T, OneOrMany<Embedding>)>, EmbeddingError> {
        let (result, _usage) = self.build_with_usage().await?;
        Ok(result)
    }

    /// Generate embeddings for all documents in the builder and return accumulated token usage.
    ///
    /// Returns `(document, embeddings)` pairs and the total token usage across all
    /// batches. A document may produce one or many embeddings depending on how its
    /// [`Embed`] implementation uses [`TextEmbedder`].
    pub async fn build_with_usage(
        self,
    ) -> Result<(Vec<(T, OneOrMany<Embedding>)>, Usage), EmbeddingError> {
        use stream::TryStreamExt;

        // Assign every text a global slot before chunking, and record how many
        // slots each document owns. Batches complete in whatever order the
        // provider finishes them (`buffer_unordered`), so when a batch
        // completes must not affect where its embeddings land: a finished
        // batch writes each embedding into its own slot, and reassembly walks
        // the slots in order, handing each document its run.
        let mut docs = Vec::with_capacity(self.documents.len());
        let mut texts = Vec::new();
        for (doc, doc_texts) in self.documents {
            docs.push((doc, doc_texts.len()));
            texts.extend(doc_texts);
        }

        // Compute the embeddings.
        let (landed, usage) = stream::iter(texts.into_iter().enumerate())
            // Chunk them into batches. Each batch size is at most the embedding API limit per request.
            .chunks(M::MAX_DOCUMENTS)
            // Generate the embeddings for each batch with usage tracking.
            .map(|chunk| async {
                let (ids, batch): (Vec<_>, Vec<_>) = chunk.into_iter().unzip();

                let response: EmbeddingResponse = self.model.embed_texts_with_usage(batch).await?;
                Ok::<_, EmbeddingError>((
                    ids.into_iter().zip(response.embeddings).collect::<Vec<_>>(),
                    response.usage,
                ))
            })
            // Parallelize the embeddings generation over 10 concurrent requests
            .buffer_unordered(max(1, 1024 / M::MAX_DOCUMENTS))
            // Land each batch's embeddings in their slots and accumulate usage.
            .try_fold(
                (HashMap::<usize, Embedding>::new(), Usage::default()),
                |(mut landed, mut usage_acc), (chunk_embeddings, chunk_usage)| async move {
                    for (slot, embedding) in chunk_embeddings {
                        landed.insert(slot, embedding);
                    }
                    usage_acc += chunk_usage;
                    Ok((landed, usage_acc))
                },
            )
            .await?;

        // Merge the embeddings with their respective documents, in input
        // order: take each document's slots by walking the counter, so a
        // landing position can never depend on batch completion order. A
        // missing slot means the provider returned fewer embeddings than the
        // texts it was sent — an external defect, surfaced as a typed error.
        let mut next_slot = 0usize;
        let mut result = Vec::with_capacity(docs.len());
        for (doc, count) in docs {
            let mut embeddings = Vec::with_capacity(count);
            for slot in next_slot..next_slot + count {
                let embedding = landed.get(&slot).cloned().ok_or_else(|| {
                    crate::embeddings::EmbeddingError::ResponseError(
                        "missing embedding for document after batch merge".to_string(),
                    )
                })?;
                embeddings.push(embedding);
            }
            let embeddings = OneOrMany::many(embeddings).map_err(|_| {
                crate::embeddings::EmbeddingError::ResponseError(
                    "document produced no texts to embed".to_string(),
                )
            })?;
            result.push((doc, embeddings));
            next_slot += count;
        }

        Ok((result, usage))
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{MockEmbeddingModel, MockMultiTextDocument, MockTextDocument};

    use super::EmbeddingsBuilder;

    fn definitions_multiple_text() -> Vec<MockMultiTextDocument> {
        vec![
            MockMultiTextDocument::new(
                "doc0",
                [
                    "A green alien that lives on cold planets.",
                    "A fictional digital currency that originated in the animated series Rick and Morty.",
                ],
            ),
            MockMultiTextDocument::new(
                "doc1",
                [
                    "An ancient tool used by the ancestors of the inhabitants of planet Jiro to farm the land.",
                    "A fictional creature found in the distant, swampy marshlands of the planet Glibbo in the Andromeda galaxy.",
                ],
            ),
        ]
    }

    fn definitions_multiple_text_2() -> Vec<MockMultiTextDocument> {
        vec![
            MockMultiTextDocument::new("doc2", ["Another fake definitions"]),
            MockMultiTextDocument::new("doc3", ["Some fake definition"]),
        ]
    }

    fn definitions_single_text() -> Vec<MockTextDocument> {
        vec![
            MockTextDocument::new("doc0", "A green alien that lives on cold planets."),
            MockTextDocument::new(
                "doc1",
                "An ancient tool used by the ancestors of the inhabitants of planet Jiro to farm the land.",
            ),
        ]
    }

    #[tokio::test]
    async fn test_build_multiple_text() {
        let fake_definitions = definitions_multiple_text();

        let fake_model = MockEmbeddingModel;
        let result = EmbeddingsBuilder::new(fake_model)
            .documents(fake_definitions)
            .unwrap()
            .build()
            .await
            .unwrap();

        assert_eq!(result.len(), 2);

        let first_definition = &result[0];
        assert_eq!(first_definition.0.id, "doc0");
        assert_eq!(first_definition.1.len(), 2);
        assert_eq!(
            first_definition.1.first().document,
            "A green alien that lives on cold planets.".to_string()
        );

        let second_definition = &result[1];
        assert_eq!(second_definition.0.id, "doc1");
        assert_eq!(second_definition.1.len(), 2);
        assert_eq!(
            second_definition.1.rest()[0].document, "A fictional creature found in the distant, swampy marshlands of the planet Glibbo in the Andromeda galaxy.".to_string()
        )
    }

    #[tokio::test]
    async fn test_build_single_text() {
        let fake_definitions = definitions_single_text();

        let fake_model = MockEmbeddingModel;
        let result = EmbeddingsBuilder::new(fake_model)
            .documents(fake_definitions)
            .unwrap()
            .build()
            .await
            .unwrap();

        assert_eq!(result.len(), 2);

        let first_definition = &result[0];
        assert_eq!(first_definition.0.id, "doc0");
        assert_eq!(first_definition.1.len(), 1);
        assert_eq!(
            first_definition.1.first().document,
            "A green alien that lives on cold planets.".to_string()
        );

        let second_definition = &result[1];
        assert_eq!(second_definition.0.id, "doc1");
        assert_eq!(second_definition.1.len(), 1);
        assert_eq!(
            second_definition.1.first().document, "An ancient tool used by the ancestors of the inhabitants of planet Jiro to farm the land.".to_string()
        )
    }

    #[tokio::test]
    async fn test_build_multiple_and_single_text() {
        let fake_definitions = definitions_multiple_text();
        let fake_definitions_single = definitions_multiple_text_2();

        let fake_model = MockEmbeddingModel;
        let result = EmbeddingsBuilder::new(fake_model)
            .documents(fake_definitions)
            .unwrap()
            .documents(fake_definitions_single)
            .unwrap()
            .build()
            .await
            .unwrap();

        assert_eq!(result.len(), 4);

        let second_definition = &result[1];
        assert_eq!(second_definition.0.id, "doc1");
        assert_eq!(second_definition.1.len(), 2);
        assert_eq!(
            second_definition.1.first().document, "An ancient tool used by the ancestors of the inhabitants of planet Jiro to farm the land.".to_string()
        );

        let third_definition = &result[2];
        assert_eq!(third_definition.0.id, "doc2");
        assert_eq!(third_definition.1.len(), 1);
        assert_eq!(
            third_definition.1.first().document,
            "Another fake definitions".to_string()
        )
    }

    #[tokio::test]
    async fn test_build_string() {
        let bindings = definitions_multiple_text();
        let fake_definitions = bindings.iter().map(|def| def.texts.clone());

        let fake_model = MockEmbeddingModel;
        let result = EmbeddingsBuilder::new(fake_model)
            .documents(fake_definitions)
            .unwrap()
            .build()
            .await
            .unwrap();

        assert_eq!(result.len(), 2);

        let first_definition = &result[0];
        assert_eq!(first_definition.1.len(), 2);
        assert_eq!(
            first_definition.1.first().document,
            "A green alien that lives on cold planets.".to_string()
        );

        let second_definition = &result[1];
        assert_eq!(second_definition.1.len(), 2);
        assert_eq!(
            second_definition.1.rest()[0].document, "A fictional creature found in the distant, swampy marshlands of the planet Glibbo in the Andromeda galaxy.".to_string()
        )
    }

    /// A model whose first request is slow, so the second batch completes
    /// first under `buffer_unordered` — the completion order that used to
    /// decide where embeddings landed.
    #[derive(Clone, Default)]
    struct SlowFirstBatchModel {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::embeddings::EmbeddingModel for SlowFirstBatchModel {
        const MAX_DOCUMENTS: usize = 5;

        type Client = crate::client::Nothing;

        fn make(_: &Self::Client, _: impl Into<String>, _: Option<usize>) -> Self {
            Self::default()
        }

        fn ndims(&self) -> usize {
            1
        }

        async fn embed_texts(
            &self,
            documents: impl IntoIterator<Item = String> + crate::wasm_compat::WasmCompatSend,
        ) -> Result<Vec<crate::embeddings::Embedding>, crate::embeddings::EmbeddingError> {
            use std::sync::atomic::Ordering;
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Ok(documents
                .into_iter()
                .map(|document| crate::embeddings::Embedding {
                    document,
                    vec: vec![0.0],
                })
                .collect())
        }
    }

    /// One document with six texts under `MAX_DOCUMENTS = 5` straddles a batch
    /// boundary; with the first batch slow, the old accumulator recorded the
    /// second batch's embedding first and the document read its own texts back
    /// shuffled (`["t5", "t0", …, "t4"]`). Slots make landing position
    /// independent of completion order.
    #[tokio::test]
    async fn a_documents_embeddings_stay_in_text_order_across_batch_boundaries() {
        let doc = MockMultiTextDocument::new("doc0", ["t0", "t1", "t2", "t3", "t4", "t5"]);

        let result = EmbeddingsBuilder::new(SlowFirstBatchModel::default())
            .document(doc)
            .expect("embed the document")
            .build()
            .await
            .expect("build should succeed");

        assert_eq!(result.len(), 1);
        let (_, embeddings) = &result[0];
        let order: Vec<&str> = embeddings.iter().map(|e| e.document.as_str()).collect();
        assert_eq!(order, ["t0", "t1", "t2", "t3", "t4", "t5"]);
    }
}
