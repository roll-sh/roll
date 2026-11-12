//! One branch adapter per data product, over this node's API.

use super::*;
use async_trait::async_trait;
use reqwest::Method;
use roll_previews::Kind;
use roll_previews::Mode;
use roll_previews::ports::{Branched, Brancher, Usage};
use serde_json::{Value, json};

// ── Postgres: a branch of the project's main ──

pub struct Postgres(pub Api);

impl Postgres {
    async fn find(&self, project: &str, name: &str) -> anyhow::Result<Option<Value>> {
        let p = self.0.get(&format!("/v1/projects/{}", seg(project))).await?;
        Ok(p["branches"].as_array().and_then(|b| b.iter().find(|b| b["name"] == name).cloned()))
    }
}

#[async_trait]
impl Brancher for Postgres {
    fn kind(&self) -> Kind {
        Kind::Postgres
    }

    async fn find(&self, parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        match self.find(parent, name).await {
            Ok(b) => Ok(b.and_then(|b| s(&b["id"]))),
            Err(e) if is_404(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `Mode::Schema`: branched, then every user table emptied (`schema_only`).
    async fn branch(&self, parent: &str, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        if let Some(b) = self.find(parent, name).await? {
            let id = s(&b["id"]).unwrap_or_default();
            // From before a crash: it may still hold rows, so empty it again (idempotent).
            if mode == Mode::Schema {
                self.0.post(&format!("/v1/projects/{}/branches/{}/reset", seg(parent), seg(&id)), json!({ "schema_only": true })).await?;
            }
            return Ok(Branched::plain(id));
        }
        match self.0.post(&format!("/v1/projects/{}/branches", seg(parent)), json!({ "name": name, "schema_only": mode == Mode::Schema })).await {
            Ok(v) => Ok(Branched::plain(s(&v["branch"]["id"]).ok_or_else(|| anyhow::anyhow!("a branch without an id"))?)),
            Err(e) if conflict_is_ours(&e) => {
                let b = self.find(parent, name).await?.ok_or(e)?;
                Ok(Branched::plain(s(&b["id"]).unwrap_or_default()))
            }
            Err(e) => Err(e),
        }
    }

    /// In place: same branch, endpoint and `DATABASE_URL`.
    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        match self.0.post(&format!("/v1/projects/{}/branches/{}/reset", seg(parent), seg(branch)), json!({ "schema_only": mode == Mode::Schema })).await {
            Ok(_) => Ok(Branched::plain(branch)),
            // Gone (or an older node without the route): branch again.
            Err(e) if is_404(&e) => {
                self.delete(parent, branch, name).await?;
                self.branch(parent, name, mode).await
            }
            Err(e) => Err(e),
        }
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        let p = match self.0.get(&format!("/v1/projects/{}", seg(parent))).await {
            Ok(p) => p,
            Err(e) if is_404(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        let Some(b) = p["branches"].as_array().and_then(|bs| bs.iter().find(|b| b["id"] == branch).cloned()) else { return Ok(()) };
        anyhow::ensure!(b["name"] == name && !b["parent_id"].is_null(), "refusing to delete Postgres branch {branch}: it isn't {parent}'s branch {name}");
        self.0.delete(&format!("/v1/projects/{}/branches/{}", seg(parent), seg(branch))).await
    }

    async fn connect(&self, parent: &str, branch: &str, _kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let p = self.0.get(&format!("/v1/projects/{}", seg(parent))).await?;
        let b = p["branches"].as_array().and_then(|bs| bs.iter().find(|b| b["id"] == branch).cloned()).ok_or_else(|| anyhow::anyhow!("no Postgres branch {branch}"))?;
        let uri = s(&b["connection_uri"]).ok_or_else(|| anyhow::anyhow!("Postgres branch {branch} has no endpoint"))?;
        Ok(vec![("DATABASE_URL".into(), uri)])
    }

    /// Evict the branch: its local data directory goes to object storage (a base backup); the
    /// next connection restores it. On ext4 this is what makes an idle Postgres branch cheap.
    async fn idle(&self, parent: &str, branch: &str) -> anyhow::Result<()> {
        match self.0.post(&format!("/v1/projects/{}/branches/{}/evict", seg(parent), seg(branch)), json!({})).await {
            Ok(_) => Ok(()),
            Err(e) if is_404(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// A branch's own bytes (its local data directory, else its objects), at Postgres's rates.
    async fn usage(&self, parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let p = self.0.get(&format!("/v1/projects/{}", seg(parent))).await?;
        let u = self.0.get(&format!("/v1/projects/{}/usage?days=1", seg(parent))).await?;
        let price = |k: &str, d: f64| u["pricing"][k].as_f64().unwrap_or(d);
        let b = p["branches"].as_array().and_then(|bs| bs.iter().find(|b| b["id"] == branch).cloned()).unwrap_or(Value::Null);
        let local = b["storage_bytes"].as_i64().unwrap_or(0);
        let objects = b["object_bytes"].as_i64().unwrap_or(0);
        let storage = gb(local) * price("usd_per_gb_month", 0.08) + gb(objects) * price("usd_per_gb_month_s3", 0.023);
        let cu = b["endpoint"]["min_cu"].as_f64().unwrap_or(0.25).max(0.25);
        Ok(Usage { bytes: local.max(objects), storage_usd_month: storage, active_usd_hour: cu * price("usd_per_cu_hour", 0.0408), month_to_date_usd: 0.0 })
    }
}

// ── Redis ──

pub struct Redis(pub Api);

#[async_trait]
impl Brancher for Redis {
    fn kind(&self) -> Kind {
        Kind::Redis
    }

    async fn find(&self, parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        let d = match self.0.get(&format!("/v1/redis/databases/{}", seg(name))).await {
            Ok(d) => d,
            Err(e) if is_404(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let parent_id = self.0.get(&format!("/v1/redis/databases/{}", seg(parent))).await.map(|p| p["id"].clone()).unwrap_or(Value::Null);
        Ok((d["parent_id"] == parent_id && !parent_id.is_null()).then(|| s(&d["id"])).flatten())
    }

    async fn branch(&self, parent: &str, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        anyhow::ensure!(mode == Mode::Branch, "a Redis branch can't be structure-only");
        match self.0.post(&format!("/v1/redis/databases/{}/branches", seg(parent)), json!({ "name": name })).await {
            Ok(v) => Ok(Branched::plain(s(&v["id"]).ok_or_else(|| anyhow::anyhow!("a database without an id"))?)),
            Err(e) if conflict_is_ours(&e) => {
                // Names are global: the database named `name` is ours only if it's the parent's.
                let d = self.0.get(&format!("/v1/redis/databases/{}", seg(name))).await?;
                let parent_id = self.0.get(&format!("/v1/redis/databases/{}", seg(parent))).await?["id"].clone();
                anyhow::ensure!(d["parent_id"] == parent_id, "a Redis database named {name} exists and isn't a branch of {parent}");
                Ok(Branched::plain(s(&d["id"]).unwrap_or_default()))
            }
            Err(e) => Err(e),
        }
    }

    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        let d = match self.0.get(&format!("/v1/redis/databases/{}", seg(branch))).await {
            Ok(d) => d,
            Err(e) if is_404(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        let parent_id = match self.0.get(&format!("/v1/redis/databases/{}", seg(parent))).await {
            Ok(p) => p["id"].clone(),
            // The parent is gone: the branch still must carry our name.
            Err(e) if is_404(&e) => d["parent_id"].clone(),
            Err(e) => return Err(e),
        };
        anyhow::ensure!(d["name"] == name && d["parent_id"] == parent_id && !parent_id.is_null(), "refusing to delete Redis database {branch}: it isn't {parent}'s branch {name}");
        self.0.delete(&format!("/v1/redis/databases/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, _kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let d = self.0.get(&format!("/v1/redis/databases/{}", seg(branch))).await?;
        Ok(vec![("REDIS_URL".into(), s(&d["redis_url"]).ok_or_else(|| anyhow::anyhow!("no redis_url"))?)])
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let u = self.0.get(&format!("/v1/redis/databases/{}/usage?days=30", seg(branch))).await?;
        let d = self.0.get(&format!("/v1/redis/databases/{}", seg(branch))).await?;
        let bytes = u["storage_bytes"].as_i64().unwrap_or(0);
        let mb = d["max_memory_mb"].as_f64().unwrap_or(256.0);
        Ok(Usage {
            bytes,
            storage_usd_month: gb(bytes) * 0.023,
            // Memory is billed while loaded: a busy preview fills it up to its peak.
            active_usd_hour: (bytes as f64 / (1u64 << 30) as f64).max(mb.min(64.0) / 1024.0) * 0.0067,
            month_to_date_usd: u["cost"]["total_usd"].as_f64().unwrap_or(0.0),
        })
    }
}

// ── NoSQL ──

pub struct Nosql(pub Api);

#[async_trait]
impl Brancher for Nosql {
    fn kind(&self) -> Kind {
        Kind::Nosql
    }

    async fn find(&self, parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        let d = match self.0.get(&format!("/v1/nosql/databases/{}", seg(name))).await {
            Ok(d) => d,
            Err(e) if is_404(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let parent_id = self.0.get(&format!("/v1/nosql/databases/{}", seg(parent))).await.map(|p| p["id"].clone()).unwrap_or(Value::Null);
        Ok((d["parent_id"] == parent_id && !parent_id.is_null()).then(|| s(&d["id"])).flatten())
    }

    async fn branch(&self, parent: &str, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        let body = json!({ "name": name, "schema_only": mode == Mode::Schema });
        match self.0.post(&format!("/v1/nosql/databases/{}/branches", seg(parent)), body).await {
            Ok(v) => Ok(Branched::plain(s(&v["id"]).ok_or_else(|| anyhow::anyhow!("a database without an id"))?)),
            Err(e) if conflict_is_ours(&e) => {
                let d = self.0.get(&format!("/v1/nosql/databases/{}", seg(name))).await?;
                let parent_id = self.0.get(&format!("/v1/nosql/databases/{}", seg(parent))).await?["id"].clone();
                anyhow::ensure!(d["parent_id"] == parent_id, "a NoSQL database named {name} exists and isn't a branch of {parent}");
                Ok(Branched::plain(s(&d["id"]).unwrap_or_default()))
            }
            Err(e) => Err(e),
        }
    }

    /// Merge masks onto the branch's own policy (inherited from its parent at branching).
    async fn mask(&self, branch: &str, columns: &std::collections::BTreeMap<String, String>) -> anyhow::Result<()> {
        let d = self.0.get(&format!("/v1/nosql/databases/{}", seg(branch))).await?;
        let mut policy = d["masking"].as_object().cloned().unwrap_or_default();
        for (col, f) in columns {
            policy.insert(col.clone(), json!(f));
        }
        self.0.call(Method::PATCH, &format!("/v1/nosql/databases/{}", seg(branch)), Some(json!({ "masking": policy }))).await.map(|_| ())
    }

    /// NoSQL resets in place: same id, endpoint and credentials.
    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        match self.0.post(&format!("/v1/nosql/databases/{}/reset", seg(branch)), json!({})).await {
            Ok(_) => Ok(Branched::plain(branch)),
            Err(e) if is_404(&e) => self.branch(parent, name, mode).await,
            Err(e) => Err(e),
        }
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        let d = match self.0.get(&format!("/v1/nosql/databases/{}", seg(branch))).await {
            Ok(d) => d,
            Err(e) if is_404(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        let parent_id = match self.0.get(&format!("/v1/nosql/databases/{}", seg(parent))).await {
            Ok(p) => p["id"].clone(),
            Err(e) if is_404(&e) => d["parent_id"].clone(),
            Err(e) => return Err(e),
        };
        anyhow::ensure!(d["name"] == name && d["parent_id"] == parent_id && !parent_id.is_null(), "refusing to delete NoSQL database {branch}: it isn't {parent}'s branch {name}");
        self.0.delete(&format!("/v1/nosql/databases/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, _kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let d = self.0.get(&format!("/v1/nosql/databases/{}", seg(branch))).await?;
        let mut v = vec![
            ("CASSANDRA_CONTACT_POINTS".into(), s(&d["endpoints"]["cql"]).unwrap_or_default()),
            ("CASSANDRA_USERNAME".into(), s(&d["credentials"]["username"]).unwrap_or_default()),
            ("CASSANDRA_PASSWORD".into(), s(&d["credentials"]["password"]).unwrap_or_default()),
        ];
        if let Some(dc) = s(&d["datacenter"]) {
            v.push(("CASSANDRA_LOCAL_DATACENTER".into(), dc));
        }
        Ok(v)
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let u = self.0.get(&format!("/v1/nosql/databases/{}/usage?days=30", seg(branch))).await?;
        let bytes = u["storage_bytes"].as_i64().unwrap_or(0);
        Ok(Usage { bytes, storage_usd_month: gb(bytes) * 0.023, active_usd_hour: 0.0, month_to_date_usd: u["cost"]["total_usd"].as_f64().unwrap_or(0.0) })
    }
}

// ── Storage buckets: a copy-on-write branch plus a write key scoped to it ──

pub struct Bucket(pub Api);

impl Bucket {
    /// A fresh write key for the branch only (the secret is shown once, so it's kept).
    async fn key(&self, bucket: &str) -> anyhow::Result<String> {
        let k = self.0.post("/v1/storage/keys", json!({ "name": bucket, "permission": "write", "buckets": [bucket] })).await?;
        let info = self.0.get("/v1/storage").await.unwrap_or(Value::Null);
        Ok(json!({
            "S3_BUCKET": bucket,
            "AWS_ENDPOINT_URL_S3": info["endpoint"],
            "AWS_REGION": info["region"],
            "AWS_ACCESS_KEY_ID": k["id"],
            "AWS_SECRET_ACCESS_KEY": k["secret_access_key"],
        })
        .to_string())
    }

    /// Every key minted for the branch.
    async fn drop_keys(&self, bucket: &str) -> anyhow::Result<()> {
        let keys = self.0.get("/v1/storage/keys").await?;
        for k in keys.as_array().into_iter().flatten() {
            if k["name"] == bucket && k["buckets"].as_array().is_some_and(|b| b.len() == 1 && b[0] == bucket) {
                self.0.delete(&format!("/v1/storage/keys/{}", seg(k["id"].as_str().unwrap_or_default()))).await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Brancher for Bucket {
    fn kind(&self) -> Kind {
        Kind::Bucket
    }

    async fn find(&self, parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        let child = bucket_name(parent, name);
        match self.0.get(&format!("/v1/storage/buckets/{}", seg(&child))).await {
            Ok(b) if b["parent"]["name"] == parent => Ok(Some(child)),
            Ok(_) => Ok(None),
            Err(e) if is_404(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn branch(&self, parent: &str, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        anyhow::ensure!(mode == Mode::Branch, "a bucket branch can't be structure-only");
        // Bucket names are global: the parent's name keeps it readable, the preview's makes it
        // unique (63 characters at most).
        let child = bucket_name(parent, name);
        match self.0.post(&format!("/v1/storage/buckets/{}/branches", seg(parent)), json!({ "name": child })).await {
            Ok(_) => {}
            Err(e) if conflict_is_ours(&e) => {
                let b = self.0.get(&format!("/v1/storage/buckets/{}", seg(&child))).await?;
                anyhow::ensure!(b["parent"]["name"] == parent, "a bucket named {child} exists and isn't a branch of {parent}");
            }
            Err(e) => return Err(e),
        }
        // A key from before a crash can't be read back: replace it.
        self.drop_keys(&child).await?;
        Ok(Branched { r#ref: child.clone(), kept: Some(self.key(&child).await?) })
    }

    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(branch == bucket_name(parent, name) && branch != parent, "refusing to delete bucket {branch}: it isn't {parent}'s branch {name}");
        let b = match self.0.get(&format!("/v1/storage/buckets/{}", seg(branch))).await {
            Ok(b) => b,
            Err(e) if is_404(&e) => return self.drop_keys(branch).await,
            Err(e) => return Err(e),
        };
        anyhow::ensure!(b["parent"]["name"] == parent, "refusing to delete bucket {branch}: it isn't a branch of {parent}");
        // `force` empties the branch: only its own rows (the parent's objects are the parent's).
        self.0.delete(&format!("/v1/storage/buckets/{}?force=true", seg(branch))).await?;
        self.drop_keys(branch).await
    }

    async fn connect(&self, _parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let kept: Value = kept.and_then(|k| serde_json::from_str(k).ok()).ok_or_else(|| anyhow::anyhow!("the bucket branch {branch} has no key: reset it"))?;
        Ok(kept.as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let b = self.0.get(&format!("/v1/storage/buckets/{}", seg(branch))).await?;
        let bytes = b["bytes"].as_i64().unwrap_or(0) + b["trash_bytes"].as_i64().unwrap_or(0) + b["version_bytes"].as_i64().unwrap_or(0);
        Ok(Usage { bytes, storage_usd_month: gb(bytes) * 0.023, active_usd_hour: 0.0, month_to_date_usd: 0.0 })
    }
}

/// `<parent>-<preview branch name>`, cut to S3's 63 characters (the preview part kept whole).
pub fn bucket_name(parent: &str, name: &str) -> String {
    let room = 63usize.saturating_sub(name.len() + 1);
    let head: String = parent.chars().take(room).collect();
    format!("{}-{name}", head.trim_end_matches(['-', '.']))
}

// ── Streams: a copy-on-write branch (`branch`), or a stream with the parent's topics (`schema`) ──

pub struct Stream(pub Api);

#[async_trait]
impl Brancher for Stream {
    fn kind(&self) -> Kind {
        Kind::Stream
    }

    async fn find(&self, _parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        match self.0.get(&format!("/v1/streams/{}", seg(name))).await {
            Ok(v) if v["name"] == name => Ok(s(&v["id"])),
            Ok(_) => Ok(None),
            Err(e) if is_404(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn branch(&self, parent: &str, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        let p = self.0.get(&format!("/v1/streams/{}", seg(parent))).await?;
        // `branch`: the parent's records (shared chunks) and committed offsets, on the parent's
        // owner. `schema`: a new stream, then the parent's topics.
        let made = match mode {
            Mode::Branch => self.0.post(&format!("/v1/streams/{}/branches", seg(parent)), json!({ "name": name })).await,
            Mode::Schema => self.0.post("/v1/streams", json!({ "name": name, "settings": p["settings"] })).await,
        };
        let made = match made {
            Ok(v) => v,
            Err(e) if conflict_is_ours(&e) => {
                // From before a crash: a fresh key, since the first secret is gone.
                let v = self.0.get(&format!("/v1/streams/{}", seg(name))).await?;
                let key = self.0.post(&format!("/v1/streams/{}/keys", seg(name)), json!({ "label": "roll preview" })).await?;
                json!({ "id": v["id"], "bootstrap": v["bootstrap"], "connect": { "bootstrap": v["bootstrap"], "username": key["username"], "secret": key["secret"] } })
            }
            Err(e) => return Err(e),
        };
        let id = s(&made["id"]).ok_or_else(|| anyhow::anyhow!("a stream without an id"))?;
        // A branch has its topics already (and a stream from before a crash is either whole or
        // gets the missing ones here).
        for t in self.0.get(&format!("/v1/streams/{}/topics", seg(parent))).await?.as_array().into_iter().flatten() {
            let body = json!({ "name": t["name"], "partitions": t["partitions"], "configs": t["configs"] });
            match self.0.post(&format!("/v1/streams/{}/topics", seg(&id)), body).await {
                Ok(_) => {}
                Err(e) if conflict_is_ours(&e) => {}
                Err(e) => return Err(e),
            }
        }
        let c = &made["connect"];
        let kept = json!({
            "KAFKA_BOOTSTRAP_SERVERS": c["bootstrap"],
            "KAFKA_SASL_MECHANISM": "SCRAM-SHA-256",
            "KAFKA_SASL_USERNAME": c["username"],
            "KAFKA_SASL_PASSWORD": c["secret"],
        });
        Ok(Branched { r#ref: id, kept: Some(kept.to_string()) })
    }

    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        let v = match self.0.get(&format!("/v1/streams/{}", seg(branch))).await {
            Ok(v) => v,
            Err(e) if is_404(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        anyhow::ensure!(v["name"] == name && branch != parent, "refusing to delete stream {branch}: it isn't the preview's stream {name}");
        if let Some(of) = s(&v["branch_of"]["stream"]) {
            match self.0.get(&format!("/v1/streams/{}", seg(parent))).await {
                Ok(p) => anyhow::ensure!(p["id"] == of.as_str(), "refusing to delete stream {branch}: it's a branch of {of}, not of {parent}"),
                Err(e) if is_404(&e) => {}
                Err(e) => return Err(e),
            }
        }
        self.0.delete(&format!("/v1/streams/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let kept: Value = kept.and_then(|k| serde_json::from_str(k).ok()).ok_or_else(|| anyhow::anyhow!("the stream {branch} has no key: reset it"))?;
        Ok(kept.as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        // Object storage counts only the branch's own bytes: what it shares with its parent is
        // the parent's.
        let u = self.0.get(&format!("/v1/streams/{}/usage?hours=24", seg(branch))).await?;
        let last = u["storage"].as_array().and_then(|a| a.last()).cloned().unwrap_or(Value::Null);
        let bytes = last["local_bytes"].as_i64().unwrap_or(0) + u["remote"]["own_bytes"].as_i64().unwrap_or(0);
        Ok(Usage { bytes, storage_usd_month: gb(bytes) * 0.023, active_usd_hour: 0.0, month_to_date_usd: 0.0 })
    }
}

// ── Queues: a vhost with the parent's definitions, no messages ──

pub struct Queue(pub Api);

#[async_trait]
impl Brancher for Queue {
    fn kind(&self) -> Kind {
        Kind::Queue
    }

    async fn find(&self, _parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        let vhosts = self.0.get("/v1/queues/vhosts").await?;
        Ok(vhosts.as_array().is_some_and(|a| a.iter().any(|v| v["name"] == name)).then(|| name.to_string()))
    }

    async fn branch(&self, parent: &str, name: &str, _mode: Mode) -> anyhow::Result<Branched> {
        let vhosts = self.0.get("/v1/queues/vhosts").await?;
        let parent_v = vhosts.as_array().and_then(|a| a.iter().find(|v| v["name"] == parent).cloned()).ok_or_else(|| anyhow::anyhow!("no vhost {parent}"))?;
        let exists = vhosts.as_array().is_some_and(|a| a.iter().any(|v| v["name"] == name));
        if !exists {
            let body = json!({ "name": name, "description": format!("roll preview of {parent}"), "default_queue_type": parent_v["default_queue_type"] });
            match self.0.post("/v1/queues/vhosts", body).await {
                Ok(_) => {}
                Err(e) if conflict_is_ours(&e) => {}
                Err(e) => return Err(e),
            }
        }
        // The parent's exchanges, queues, bindings, policies and parameters, moved to the branch.
        let defs = self.0.get("/v1/queues/definitions").await?;
        let mut copy = json!({});
        for key in ["exchanges", "queues", "bindings", "policies", "parameters"] {
            let items: Vec<Value> = defs[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|i| i["vhost"] == parent)
                // Shovels and federation move messages in from elsewhere (production, with its
                // credentials): a branch never gets them.
                .filter(|i| !(key == "parameters" && i["component"].as_str().is_some_and(|c| c.starts_with("shovel") || c.starts_with("federation"))))
                .map(|i| {
                    let mut i = i.clone();
                    i["vhost"] = json!(name);
                    if key == "policies"
                        && let Some(def) = i["definition"].as_object_mut()
                    {
                        def.retain(|k, _| !k.starts_with("federation-upstream"));
                    }
                    i
                })
                .collect();
            copy[key] = json!(items);
        }
        self.0.post("/v1/queues/definitions", copy).await?;
        let c = self.0.post(&format!("/v1/queues/vhosts/{}/credentials", seg(name)), json!({})).await?;
        Ok(Branched { r#ref: name.to_string(), kept: Some(json!({ "AMQP_URL": c["urls"]["amqp"] }).to_string()) })
    }

    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(branch == name && branch != parent, "refusing to delete vhost {branch}: it isn't the preview's vhost {name}");
        self.0.delete(&format!("/v1/queues/vhosts/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let kept: Value = kept.and_then(|k| serde_json::from_str(k).ok()).ok_or_else(|| anyhow::anyhow!("the vhost {branch} has no credentials: reset it"))?;
        Ok(kept.as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let vhosts = self.0.get("/v1/queues/vhosts").await?;
        let bytes = vhosts.as_array().and_then(|a| a.iter().find(|v| v["name"] == branch)).and_then(|v| v["stored_bytes"].as_i64()).unwrap_or(0);
        Ok(Usage { bytes, storage_usd_month: gb(bytes) * 0.023, active_usd_hour: 0.0, month_to_date_usd: 0.0 })
    }
}

// ── Tasks: an empty tenant (copied schedules would call production) ──

pub struct Tasks {
    pub api: Api,
    /// Where services reach the tasks API.
    pub public: String,
}

#[async_trait]
impl Brancher for Tasks {
    fn kind(&self) -> Kind {
        Kind::Tasks
    }

    async fn find(&self, _parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        let t = self.api.get("/_roll/tenants").await?;
        Ok(t["tenants"].as_array().is_some_and(|a| a.iter().any(|x| x == name)).then(|| name.to_string()))
    }

    async fn branch(&self, _parent: &str, name: &str, _mode: Mode) -> anyhow::Result<Branched> {
        let token = match self.api.post("/_roll/tenants", json!({ "id": name })).await {
            Ok(v) => v["token"].clone(),
            Err(e) if conflict_is_ours(&e) => self.api.post(&format!("/_roll/tenants/{}/keys", seg(name)), json!({ "name": "roll preview" })).await?["token"].clone(),
            Err(e) => return Err(e),
        };
        let kept = json!({ "ROLL_TASKS_URL": format!("{}/t/{name}", self.public.trim_end_matches('/')), "ROLL_TASKS_TOKEN": token });
        Ok(Branched { r#ref: name.to_string(), kept: Some(kept.to_string()) })
    }

    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(branch == name && branch != parent && branch != "default", "refusing to delete tasks tenant {branch}: it isn't the preview's tenant {name}");
        self.api.delete(&format!("/_roll/tenants/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let kept: Value = kept.and_then(|k| serde_json::from_str(k).ok()).ok_or_else(|| anyhow::anyhow!("the tenant {branch} has no key: reset it"))?;
        Ok(kept.as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
    }

    async fn usage(&self, _parent: &str, _branch: &str) -> anyhow::Result<Usage> {
        Ok(Usage::default())
    }
}

// ── Search: a zero-copy branch of an Elasticsearch-compatible cluster (roll search) ──

pub struct Search(pub Api);

impl Search {
    /// `url` with the branch's credentials in it (the password is shown once, so it's kept).
    fn kept(v: &Value) -> anyhow::Result<String> {
        let url = s(&v["url"]).ok_or_else(|| anyhow::anyhow!("a search branch without a url"))?;
        let (user, pass) = (s(&v["username"]).unwrap_or_default(), s(&v["password"]).unwrap_or_default());
        let with = match url.split_once("://") {
            Some((scheme, rest)) if !user.is_empty() => format!("{scheme}://{}:{}@{rest}", seg(&user), seg(&pass)),
            _ => url,
        };
        Ok(json!({ "ELASTICSEARCH_URL": with }).to_string())
    }
}

#[async_trait]
impl Brancher for Search {
    fn kind(&self) -> Kind {
        Kind::Search
    }

    async fn find(&self, parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        match self.0.get(&format!("/v1/search/clusters/{}", seg(name))).await {
            Ok(c) if c["name"] == name && (c["parent_id"] == parent || c["parent_name"] == parent) => Ok(s(&c["id"])),
            Ok(_) => Ok(None),
            Err(e) if is_404(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn branch(&self, parent: &str, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        let path = format!("/v1/search/clusters/{}/branches", seg(parent));
        match self.0.post(&path, json!({ "name": name })).await {
            Ok(v) => Ok(Branched { r#ref: s(&v["id"]).ok_or_else(|| anyhow::anyhow!("a search branch without an id"))?, kept: Some(Self::kept(&v)?) }),
            // From before a crash: its password is gone, and it holds nothing yet. Make it again.
            Err(e) if conflict_is_ours(&e) => {
                let c = self.0.get(&format!("/v1/search/clusters/{}", seg(name))).await?;
                self.delete(parent, &s(&c["id"]).unwrap_or_default(), name).await?;
                let v = self.0.post(&path, json!({ "name": name })).await?;
                let _ = mode;
                Ok(Branched { r#ref: s(&v["id"]).unwrap_or_default(), kept: Some(Self::kept(&v)?) })
            }
            Err(e) => Err(e),
        }
    }

    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        let c = match self.0.get(&format!("/v1/search/clusters/{}", seg(branch))).await {
            Ok(c) => c,
            Err(e) if is_404(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        let of_parent = c["parent_id"] == parent || c["parent_name"] == parent;
        anyhow::ensure!(c["name"] == name && of_parent && branch != parent, "refusing to delete search cluster {branch}: it isn't {parent}'s branch {name}");
        self.0.delete(&format!("/v1/search/clusters/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let kept: Value = kept.and_then(|k| serde_json::from_str(k).ok()).ok_or_else(|| anyhow::anyhow!("the search branch {branch} has no credentials: reset it"))?;
        Ok(kept.as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let c = self.0.get(&format!("/v1/search/clusters/{}", seg(branch))).await?;
        // What the branch wrote, when roll search says (`own_bytes`); else everything it
        // references, the parent's shared segments included (an overestimate).
        let bytes = c["own_bytes"].as_i64().or_else(|| c["stored_bytes"].as_i64()).unwrap_or(0);
        Ok(Usage { bytes, storage_usd_month: gb(bytes) * 0.023, active_usd_hour: 0.0, month_to_date_usd: 0.0 })
    }
}

#[cfg(test)]
mod tests {
    use super::bucket_name;

    #[test]
    fn bucket_branches_are_named_after_their_parent() {
        assert_eq!(bucket_name("assets", "pv-abc"), "assets-pv-abc");
        let long = "a".repeat(80);
        let n = bucket_name(&long, "pv-abcdefghijkl");
        assert_eq!(n.len(), 63);
        assert!(n.ends_with("-pv-abcdefghijkl"));
    }
}
