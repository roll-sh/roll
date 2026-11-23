//! The inference services against recordings of Elasticsearch 9.5.5 (no test calls a real
//! provider). Each fixture in `tests/fixtures/inference/` is a scenario recorded on a private
//! Elasticsearch 9.5.5 whose provider hosts pointed at a mock: every request, Elasticsearch's
//! status and answer, and every request it sent upstream with the mock's answer. Replaying a
//! scenario against roll, with a mock that gives the same answers, checks that roll sends the
//! same requests upstream (method, path, body byte for byte, headers apart from transport
//! headers and signatures) and answers the same (steps marked `differs` compare the status
//! only: their difference is listed in docs/search/compatibility.md).

mod azure;
mod bedrock;
mod custom;
mod google;
mod mock;
mod openai_compatible;
mod streams;

pub use mock::*;
