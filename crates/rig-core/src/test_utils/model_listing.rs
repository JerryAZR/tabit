//! Model-listing helpers for deterministic tests.

use crate::{
    client::ModelLister,
    model::{Model, ModelList, ModelListingError},
    wasm_compat::WasmCompatSend,
};

/// A [`ModelLister`] that returns a preconfigured list of models.
pub struct MockModelLister {
    models: Vec<Model>,
}

impl MockModelLister {
    /// Create a model lister from a fixed model list.
    pub fn new(models: Vec<Model>) -> Self {
        Self { models }
    }
}

impl ModelLister for MockModelLister {
    type Client = Vec<Model>;

    fn new(client: Self::Client) -> Self {
        Self { models: client }
    }

    fn list_all(
        &self,
    ) -> impl std::future::Future<Output = Result<ModelList, ModelListingError>> + WasmCompatSend
    {
        let models = self.models.clone();
        async move { Ok(ModelList::new(models)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn model_lister_constructor_round_trips_through_list_all() {
        let lister =
            <MockModelLister as ModelLister>::new(vec![Model::from_id("m-1"), Model::from_id("m-2")]);

        let list = lister.list_all().await.unwrap();

        assert_eq!(list.data.len(), 2);
        assert_eq!(list.data[0].id, "m-1");
        assert_eq!(list.data[1].id, "m-2");
    }
}
