//! Aggregate data types. Each type owns its encodings (compact `Pack` when small, a real
//! structure when large), keeps an O(1) `mem()` estimate and reports `OBJECT ENCODING` names
//! that match Redis.

pub mod array;
pub mod hash;
pub mod list;
pub mod set;
pub mod stream;
pub mod template;
pub mod zset;

pub use array::Array;
pub use hash::Hash;
pub use list::List;
pub use set::Set;
pub use stream::Stream;
pub use zset::ZSet;
