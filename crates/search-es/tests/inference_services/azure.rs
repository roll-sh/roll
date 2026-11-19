//! `azureopenai`: embeddings, completions and streamed chat with an API key or an Entra ID
//! token; create, infer, update, delete and the error cases.

use crate::*;

#[tokio::test]
async fn azure_openai_matches_elasticsearch_and_hides_its_credentials() {
    let r = replay("azureopenai", false).await;
    assert_none(&r.problems);
    assert_secrets_hidden(&r, &["az-key-1", "entra-tok-1", "az-key-2", "az-key-3", "ent-2"]);
}
