//! GetObjectTorrent: `GET /<bucket>/<key>?torrent`, a BitTorrent metainfo file for the object.
//!
//! The pieces (SHA-1 of every 256 KiB) come from streaming the object once; they're kept by
//! blob (`s3_store::torrent`), since blobs never change, so later requests for it (any key,
//! copy, branch or version sharing the blob) read no data. As on S3, objects over 5 GB and
//! SSE-C objects have no torrent. `announce` is `Config::torrent_announce` (none by default:
//! trackerless). The `info` dictionary carries `x-amz-bucket` and `x-amz-key`, as S3's does.

use crate::auth::Need;
use crate::ctx::Ctx;
use crate::error::StoreResult;
use crate::response::{Reply, set};
use crate::versioning;
use axum::http::StatusCode;
use futures::StreamExt;
use s3_proto::torrent::{Metainfo, PIECE_LENGTH, PieceHasher, file_name};
use s3_proto::{Code, S3Error};
use s3_store::torrent::TorrentPieces;
use s3_store::{Bucket, Class, Encryption, ObjectMeta};

/// S3 makes torrents of objects up to 5 GB.
const MAX_TORRENT_OBJECT: u64 = 5_000_000_000;

/// The object's pieces: cached by blob, else computed by reading it once (and cached).
async fn pieces(ctx: &Ctx, meta: &ObjectMeta) -> Result<Vec<u8>, S3Error> {
    let store = ctx.store();
    if let Some(t) = store.torrent_pieces(&meta.blob_id, PIECE_LENGTH).s3()? {
        return Ok(t.pieces);
    }
    let mut hasher = PieceHasher::new(PIECE_LENGTH);
    if meta.size > 0 {
        let (_, mut stream) = store.open_blob(meta, None).await.s3()?;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                tracing::warn!(error = %e, key = meta.key, "reading an object for its torrent failed");
                S3Error::new(Code::InternalError)
            })?;
            hasher.update(&chunk);
        }
    }
    let (pieces, seen) = hasher.finish();
    if seen != meta.size {
        tracing::error!(key = meta.key, blob = meta.blob_id, seen, size = meta.size, "an object's blob has the wrong size");
        return Err(S3Error::new(Code::InternalError));
    }
    if let Err(e) = store.cache_torrent_pieces(&meta.blob_id, &TorrentPieces { piece_length: PIECE_LENGTH, pieces: pieces.clone() }) {
        tracing::debug!(error = %e, "caching torrent pieces failed");
    }
    Ok(pieces)
}

pub(crate) async fn get(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Read)?;
    let vid = versioning::version_id(ctx)?;
    let (meta, enc) = ctx.store().get_version_encryption(&bucket.name, ctx.key_name(), vid.as_deref()).s3()?;
    ctx.meter(&bucket.id, Class::B);
    if matches!(enc, Some(Encryption::Customer { .. })) {
        return Err(S3Error::msg(Code::InvalidRequest, "Torrents aren't available for objects encrypted with customer-provided keys (SSE-C)."));
    }
    if meta.size > MAX_TORRENT_OBJECT {
        return Err(S3Error::msg(Code::InvalidRequest, "Torrents are only available for objects of at most 5 GB."));
    }
    let pieces = pieces(ctx, &meta).await?;
    let name = file_name(&meta.key);
    let m = Metainfo { announce: ctx.server.cfg.torrent_announce.as_deref(), name, length: meta.size, piece_length: PIECE_LENGTH, pieces: &pieces, bucket: &bucket.name, key: &meta.key };
    let body = m.encode();
    ctx.store().meter.egress(&bucket.id, body.len() as u64, ctx.internal());
    let mut r = axum::response::Response::new(axum::body::Body::from(body));
    *r.status_mut() = StatusCode::OK;
    set(&mut r, "content-type", "application/x-bittorrent");
    let safe: String = name.chars().map(|c| if c == '"' || c == '\\' || c.is_control() || !c.is_ascii() { '_' } else { c }).collect();
    set(&mut r, "content-disposition", &format!("attachment; filename=\"{safe}.torrent\""));
    versioning::set_version(&mut r, bucket, &meta.version_id, vid.is_some());
    Ok(r)
}
