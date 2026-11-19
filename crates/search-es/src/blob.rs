//! The object stores a tenant's snapshot repository can name (`s3`, `gcs`, `azure`), behind one
//! set of operations: whole and ranged reads, writes (plain, only when absent, only when
//! unchanged), deletes, listings, and objects written in parts. Each client applies the same
//! rules to the tenant's URLs (public addresses only, no redirects or proxies, bounded reads)
//! and keeps credentials out of what it returns; errors are [`crate::s3::Error`] for all three.
//!
//! Objects in parts: an S3 multipart upload, a GCS resumable upload (every chunk but the last a
//! multiple of 256 KiB; the last names the size, which completes it), or an Azure block blob
//! (staged blocks, then the block list). Only S3 lists the uploads a crash left: GCS sessions
//! expire after a week and Azure drops uncommitted blocks after seven days, so neither leaves
//! anything a cleanup could find, as with Elasticsearch.

use crate::s3::{Download, Error};
use crate::{azure, gcs, s3};
use bytes::Bytes;

pub enum Store {
    S3(s3::Client),
    Gcs(gcs::Client),
    Azure(azure::Client),
}

macro_rules! each {
    ($self:ident, $c:ident => $e:expr) => {
        match $self {
            Store::S3($c) => $e,
            Store::Gcs($c) => $e,
            Store::Azure($c) => $e,
        }
    };
}

impl Store {
    /// The object at `key`, or `None` when there is none. More than `max` bytes is an error.
    pub async fn get(&self, key: &str, max: usize) -> Result<Option<Bytes>, Error> {
        each!(self, c => c.get(key, max).await)
    }

    /// The object at `key` and its version tag (an ETag, or a GCS generation).
    pub async fn get_tagged(&self, key: &str, max: usize) -> Result<Option<(Bytes, String)>, Error> {
        each!(self, c => c.get_tagged(key, max).await)
    }

    pub async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        each!(self, c => c.put(key, body).await)
    }

    /// Writes `key` only when there is none: `false` when one exists.
    pub async fn put_new(&self, key: &str, body: Bytes) -> Result<bool, Error> {
        each!(self, c => c.put_new(key, body).await)
    }

    /// Writes `key` only at version `tag` ([`Store::get_tagged`]'s), or only when absent with
    /// `None`: the new tag, `None` when the condition failed.
    pub async fn put_if(&self, key: &str, body: Bytes, tag: Option<&str>) -> Result<Option<String>, Error> {
        each!(self, c => c.put_if(key, body, tag).await)
    }

    /// `key` (bytes `start..end` with `range`) as it arrives, or `None` when there is none.
    pub async fn open(&self, key: &str, range: Option<(u64, u64)>) -> Result<Option<Download>, Error> {
        each!(self, c => c.open(key, range).await)
    }

    pub async fn delete(&self, key: &str) -> Result<(), Error> {
        each!(self, c => c.delete(key).await)
    }

    /// Every object under `prefix` with its size, bounded.
    pub async fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>, Error> {
        each!(self, c => c.list(prefix).await)
    }

    /// Uploads in parts in progress under `prefix` (S3 only: the others list none).
    pub async fn list_uploads(&self, prefix: &str, max: usize) -> Result<Vec<(String, String)>, Error> {
        match self {
            Store::S3(c) => c.list_uploads(prefix, max).await,
            Store::Gcs(_) | Store::Azure(_) => Ok(Vec::new()),
        }
    }

    /// Starts writing `key` in parts: the upload's id (a GCS session URL is a capability: it is
    /// never printed).
    pub async fn begin(&self, key: &str) -> Result<String, Error> {
        match self {
            Store::S3(c) => c.create_multipart(key).await,
            Store::Gcs(c) => c.begin(key).await,
            Store::Azure(_) => Ok(crate::meta::names::random_uuid().replace(['-', '_'], "")),
        }
    }

    /// Sends part `n` (from 1) of upload `id`, its bytes starting at `offset`; `total` is the
    /// object's size when it is the last part. What [`Store::complete`] needs of it.
    pub async fn upload_part(&self, key: &str, id: &str, n: u32, offset: u64, body: Bytes, total: Option<u64>) -> Result<String, Error> {
        match self {
            Store::S3(c) => c.upload_part(key, id, n, body).await,
            Store::Gcs(c) => c.upload_chunk(id, offset, body, total).await.map(|_| String::new()),
            Store::Azure(c) => c.put_block(key, id, n, body).await,
        }
    }

    /// Completes upload `id` from its parts' [`Store::upload_part`] answers, in order.
    pub async fn complete(&self, key: &str, id: &str, parts: &[String]) -> Result<(), Error> {
        match self {
            Store::S3(c) => c.complete_multipart(key, id, parts).await,
            // The last chunk completed it.
            Store::Gcs(_) => Ok(()),
            Store::Azure(c) => c.put_block_list(key, parts).await,
        }
    }

    /// Abandons upload `id`. Azure's staged blocks can't be dropped: uncommitted, they're
    /// invisible and expire.
    pub async fn abort(&self, key: &str, id: &str) -> Result<(), Error> {
        match self {
            Store::S3(c) => c.abort_multipart(key, id).await,
            Store::Gcs(c) => c.cancel(id).await,
            Store::Azure(_) => Ok(()),
        }
    }

}
