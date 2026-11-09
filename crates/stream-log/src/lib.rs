//! The storage engine behind roll streams: Kafka's record-batch format on disk, with object
//! storage as the bottomless tier.

pub mod batch;
pub mod log;
pub mod producer;

pub use log::{
    AppendError, AppendOptions, Appended, Chunk, Cleaned, CompactionPlan, CompactionPolicy, LocalRead, TailBudget, Log, LogConfig, LogStats, Read, ReadError,
    ReadPlan, Recovered, Retention, UploadPlan, read_local,
};
pub use producer::AbortedTxn;
