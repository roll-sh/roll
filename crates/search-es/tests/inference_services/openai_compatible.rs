//! `mistral`, `voyageai`, `jinaai`, `deepseek` and `hugging_face`; create, infer and the error
//! cases.

use crate::*;

#[tokio::test]
async fn openai_compatible_services_match_elasticsearch_and_hide_their_keys() {
    let r = replay("openai_compatible", false).await;
    assert_none(&r.problems);
    assert_secrets_hidden(&r, &["mi-key", "vo-key", "ji-key", "ds-key", "hf-key"]);
}
