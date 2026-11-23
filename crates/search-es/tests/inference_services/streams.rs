//! Streamed answers (`_inference/completion|chat_completion/{id}/_stream`) as they come, against
//! Elasticsearch 9.5.5's recording (`fixtures/inference/streams.json`): the mock sent each piece
//! in its own write, so each upstream read becomes the events Elasticsearch made of it; an error
//! chunk mid-stream and a connection cut before the last chunk end the stream with
//! `event: error` and no `[DONE]`, in the completion and the unified chat shapes.

use crate::*;

#[tokio::test]
async fn streamed_answers_match_elasticsearch_piece_by_piece() {
    let r = replay("streams", false).await;
    assert_none(&r.problems);
}
