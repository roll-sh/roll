//! `roll storage …`: S3-compatible object storage, through the control-plane API (`/v1/storage`).

use crate::Api;
use crate::queues::fmt::{Align::L, Align::R, Table, count, date, f, price, size, usd};
use crate::s;
use anyhow::{Context, bail};
use clap::{Subcommand, ValueEnum};
use reqwest::Method;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

mod accounts;
mod events;
mod iam;
mod roles;

#[derive(Subcommand)]
pub enum Storage {
    /// Endpoint, region, totals and prices.
    Info,
    /// List buckets.
    Buckets,
    /// Create a bucket.
    Create {
        name: String,
        /// Anyone can GET objects without a key.
        #[arg(long)]
        public: bool,
        /// Days deleted and overwritten objects stay recoverable (0–30, default 7).
        #[arg(long)]
        recover_days: Option<i64>,
        /// Turn on Object Lock (and versioning, which it needs). It can't be turned off.
        #[arg(long)]
        object_lock: bool,
    },
    /// Show a bucket: settings, versioning, size, branches.
    Show { bucket: String },
    /// Change a bucket's settings.
    Update {
        bucket: String,
        #[arg(long, conflicts_with = "private")]
        public: bool,
        #[arg(long)]
        private: bool,
        #[arg(long)]
        recover_days: Option<i64>,
        /// Allow browsers from this origin (repeatable; `*` for any). Replaces the CORS rules.
        #[arg(long = "cors-origin", value_name = "ORIGIN")]
        cors_origins: Vec<String>,
        /// Remove every CORS rule.
        #[arg(long, conflicts_with = "cors_origins")]
        no_cors: bool,
        /// Set the bucket's tags (repeatable `key=value`). Replaces them all.
        #[arg(long = "tag", value_name = "KEY=VALUE")]
        tags: Vec<String>,
        /// Remove every bucket tag.
        #[arg(long, conflicts_with = "tags")]
        no_tags: bool,
        /// Keep every version of every object (`on`), or stop making new ones (`off`:
        /// Suspended; existing versions stay). A versioned bucket can't become unversioned.
        #[arg(long, value_enum)]
        versioning: Option<Toggle>,
        /// Serve the bucket through the CDN: by key (`mutable`, purged on change), by version
        /// (`immutable`), or not (`off`).
        #[arg(long, value_enum)]
        cdn: Option<CdnArg>,
    },
    /// The URL to give out for an object: the CDN's when the bucket has it on, else S3's
    /// (presigned unless the bucket is public).
    Url {
        path: String,
        /// An immutable CDN URL (`/v/<token>/…`, cached for a year) even in a mutable bucket.
        #[arg(long)]
        immutable: bool,
        /// Presign it for this long (90s, 15m, 1h, 7d); default only for private buckets.
        #[arg(long)]
        expires: Option<String>,
    },
    /// The CDN (Cloudflare): show it, or change its settings.
    Cdn {
        /// What clients use, e.g. https://cdn.example.com (`""` removes it).
        #[arg(long)]
        base_url: Option<String>,
        /// The Cloudflare zone id.
        #[arg(long)]
        zone_id: Option<String>,
        /// A Cloudflare API token with Zone.Cache Purge (stored sealed; `""` removes it).
        #[arg(long, env = "ROLL_CDN_API_TOKEN", hide_env_values = true)]
        api_token: Option<String>,
        /// Purge changed objects by URL (`files`) or by cache tag (`tags`).
        #[arg(long)]
        purge: Option<String>,
        /// Edge TTL of mutable URLs, in seconds.
        #[arg(long)]
        mutable_ttl: Option<u32>,
        /// Print what to configure the Worker with (wrangler vars and secrets).
        #[arg(long)]
        worker: bool,
    },
    /// Delete a bucket (it must be empty unless --force).
    Delete {
        bucket: String,
        /// Delete every object first.
        #[arg(long)]
        force: bool,
    },
    /// Branch a bucket: an instant copy-on-write copy under a new name.
    Branch { bucket: String, name: String },
    /// List buckets, or objects under `<bucket>[/prefix]`.
    Ls {
        path: Option<String>,
        /// Every key under the prefix, not just one level.
        #[arg(long, short)]
        recursive: bool,
        /// Every version of every key under the prefix (noncurrent ones and delete markers too).
        #[arg(long)]
        versions: bool,
    },
    /// Copy a file to or from storage: `cp ./a.png assets/img/` or `cp assets/img/a.png -`.
    Cp {
        src: String,
        dst: String,
        /// Copy a directory (upload), or every key starting with the prefix (download; `dir`
        /// also matches `dirt.txt`, `dir/` only the directory, as `aws s3 cp --recursive`).
        #[arg(long, short)]
        recursive: bool,
        /// Content-Type for uploads (default: from the file extension).
        #[arg(long)]
        content_type: Option<String>,
        /// Download this version of the object (from `ls --versions`).
        #[arg(long, conflicts_with = "recursive")]
        version_id: Option<String>,
    },
    /// Delete an object, or with -r every object whose key starts with the prefix, as
    /// `aws s3 rm --recursive` does: `dir` also matches `dirt.txt`, `dir/` only the directory.
    Rm {
        path: String,
        #[arg(long, short)]
        recursive: bool,
        /// Delete this version for good (it moves to the trash for the bucket's recovery days),
        /// instead of adding a delete marker in a versioned bucket.
        #[arg(long, conflicts_with = "recursive")]
        version_id: Option<String>,
    },
    /// Show an object's metadata.
    Stat {
        path: String,
        /// A version of the object (from `ls --versions`).
        #[arg(long)]
        version_id: Option<String>,
    },
    /// A presigned URL for an object.
    Presign {
        path: String,
        /// A URL for uploading (PUT) instead of downloading.
        #[arg(long)]
        put: bool,
        /// How long it stays valid: 90s, 15m, 1h, 7d (max 7d).
        #[arg(long, default_value = "1h")]
        expires: String,
    },
    /// List deleted and overwritten objects that can still be restored.
    Trash {
        bucket: String,
        #[arg(long)]
        prefix: Option<String>,
    },
    /// Restore an object from the trash.
    Restore {
        path: String,
        /// The entry's `deleted_at` (Unix ms, from `roll storage trash`).
        #[arg(long)]
        at: i64,
    },
    /// Usage and cost, for one bucket or all of them.
    Usage {
        bucket: Option<String>,
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
    /// List access keys, or create / delete one.
    Keys {
        #[command(subcommand)]
        action: Option<KeysCmd>,
    },
    /// List roles (temporary credentials through STS or `sessions create`), or manage one.
    Roles {
        #[command(subcommand)]
        action: Option<roles::RolesCmd>,
    },
    /// Temporary credentials of a role.
    Sessions {
        #[command(subcommand)]
        action: roles::SessionsCmd,
    },
    /// List the deployment's accounts, manage one, or switch the account `roll storage` acts
    /// in (`accounts use <id|alias>`).
    Accounts {
        #[command(subcommand)]
        action: Option<accounts::AccountsCmd>,
    },
    /// IAM users, groups, policies and roles, and the policy simulator (as the account's root
    /// user).
    Iam {
        #[command(subcommand)]
        action: iam::IamCmd,
    },
    /// A bucket's event notifications (`notify <bucket>`), or add / remove one, or its webhook
    /// secret.
    #[command(args_conflicts_with_subcommands = true)]
    Notify {
        bucket: Option<String>,
        #[command(subcommand)]
        action: Option<events::NotifyCmd>,
    },
    /// Replicate a bucket to another roll bucket (`--to`), list its rules, or `rm` them. Inside
    /// roll a replica shares the source's data: instant, nothing copied, no transfer cost.
    #[command(args_conflicts_with_subcommands = true)]
    Replicate {
        bucket: Option<String>,
        /// The destination bucket (versioned, like the source).
        #[arg(long, value_name = "BUCKET")]
        to: Option<String>,
        /// Only keys starting with this.
        #[arg(long, requires = "to")]
        prefix: Option<String>,
        /// Replicate delete markers too.
        #[arg(long, requires = "to")]
        delete_markers: bool,
        /// Replicate the versions already in the bucket too, not just new ones.
        #[arg(long, requires = "to")]
        existing: bool,
        /// The rule's id (generated when not given).
        #[arg(long, requires = "to")]
        id: Option<String>,
        #[command(subcommand)]
        action: Option<events::ReplicateCmd>,
    },
}

#[derive(Subcommand)]
pub enum KeysCmd {
    /// Create an access key; the secret is shown once.
    Create {
        name: String,
        #[arg(long, value_enum, default_value_t = Permission::Write)]
        permission: Permission,
        /// Limit the key to this bucket (repeatable; default: every bucket).
        #[arg(long = "bucket", value_name = "BUCKET")]
        buckets: Vec<String>,
    },
    /// Delete an access key.
    Delete { id: String },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum CdnArg {
    Off,
    Mutable,
    Immutable,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Toggle {
    On,
    Off,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Permission {
    Read,
    Write,
    Admin,
}

impl Permission {
    fn as_str(self) -> &'static str {
        match self {
            Permission::Read => "read",
            Permission::Write => "write",
            Permission::Admin => "admin",
        }
    }
}

/// `bucket/key`, `bucket/prefix/` or `s3://bucket/key` → `(bucket, key)`; the key may be empty.
fn split(path: &str) -> anyhow::Result<(String, String)> {
    let p = path.strip_prefix("s3://").unwrap_or(path);
    let (bucket, key) = p.split_once('/').unwrap_or((p, ""));
    if bucket.is_empty() {
        bail!("expected <bucket>/<key>, got {path:?}");
    }
    Ok((bucket.to_string(), key.to_string()))
}

/// Like [`split`], but the key is required.
fn split_key(path: &str) -> anyhow::Result<(String, String)> {
    let (b, k) = split(path)?;
    if k.is_empty() {
        bail!("expected <bucket>/<key>, got {path:?}");
    }
    Ok((b, k))
}

fn enc(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

fn bucket_path(bucket: &str, rest: &str) -> String {
    format!("/v1/storage/buckets/{}{rest}", enc(bucket))
}

/// `90s`, `15m`, `1h`, `7d` (or plain seconds) → seconds.
fn duration(v: &str) -> anyhow::Result<i64> {
    let (n, unit) = v.split_at(v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len()));
    let n: i64 = n.parse().with_context(|| format!("bad duration {v:?}: e.g. 90s, 15m, 1h, 7d"))?;
    let unit = match unit {
        "s" | "" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => bail!("bad duration {v:?}: e.g. 90s, 15m, 1h, 7d"),
    };
    // Huge values are out of range, not an overflow.
    Ok(n.saturating_mul(unit))
}

/// Unix ms → "YYYY-MM-DD HH:MM" (UTC).
fn when(ms: &Value) -> String {
    match ms.as_i64() {
        Some(ms) => {
            let secs = ms.div_euclid(1000);
            let day = secs.rem_euclid(86_400);
            format!("{} {:02}:{:02}", date(secs), day / 3600, (day % 3600) / 60)
        }
        None => "-".into(),
    }
}

/// `?key=` (and `&version_id=`) for the object endpoints.
fn key_query(key: &str, version_id: Option<&str>) -> String {
    match version_id {
        Some(v) => format!("?key={}&version_id={}", enc(key), enc(v)),
        None => format!("?key={}", enc(key)),
    }
}

/// Days or years, from a retention's `days`/`years`.
fn period(v: &Value) -> String {
    match (v["days"].as_u64(), v["years"].as_u64()) {
        (Some(d), _) => format!("{d} day{}", if d == 1 { "" } else { "s" }),
        (_, Some(y)) => format!("{y} year{}", if y == 1 { "" } else { "s" }),
        _ => String::new(),
    }
}

/// What a lifecycle rule does and to what, in a line.
fn lifecycle_line(r: &Value) -> String {
    let mut filter = vec![format!("prefix {:?}", r["prefix"].as_str().unwrap_or(""))];
    for (k, v) in r["tags"].as_object().into_iter().flatten() {
        filter.push(format!("tag {k}={}", s(v)));
    }
    if let Some(n) = r["size_greater_than"].as_u64() {
        filter.push(format!("size > {}", size(n as f64)));
    }
    if let Some(n) = r["size_less_than"].as_u64() {
        filter.push(format!("size < {}", size(n as f64)));
    }
    let mut what = Vec::new();
    if let Some(d) = r["expire_days"].as_i64() {
        what.push(format!("expire after {d}d"));
    }
    if let Some(ms) = r["expire_at"].as_i64() {
        what.push(format!("expire on {}", date(ms.div_euclid(1000))));
    }
    if r["expire_delete_markers"].as_bool() == Some(true) {
        what.push("remove lone delete markers".into());
    }
    if let Some(d) = r["noncurrent_days"].as_i64() {
        let keep = r["newer_noncurrent_versions"].as_i64().map(|n| format!(" (keep the newest {n})")).unwrap_or_default();
        what.push(format!("delete versions noncurrent for {d}d{keep}"));
    }
    for t in r["transitions"].as_array().into_iter().flatten() {
        let at = match (t["days"].as_i64(), t["date"].as_i64()) {
            (Some(d), _) => format!(" after {d}d"),
            (_, Some(ms)) => format!(" on {}", date(ms.div_euclid(1000))),
            _ => String::new(),
        };
        what.push(format!("transition to {}{at} (stored only)", s(&t["storage_class"])));
    }
    for t in r["noncurrent_transitions"].as_array().into_iter().flatten() {
        what.push(format!("noncurrent to {} after {}d (stored only)", s(&t["storage_class"]), s(&t["noncurrent_days"])));
    }
    if let Some(d) = r["abort_multipart_days"].as_i64() {
        what.push(format!("abort uploads after {d}d"));
    }
    let off = if r["enabled"].as_bool() == Some(false) { " (disabled)" } else { "" };
    let what = if what.is_empty() { "nothing".to_string() } else { what.join(", ") };
    format!("{}: {} → {what}{off}", s(&r["id"]), filter.join(", "))
}

/// An object version's lock, in a line (`None`: not locked).
fn lock_line(l: &Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(m) = l["mode"].as_str() {
        parts.push(format!("{m} until {} UTC", when(&l["retain_until"])));
    }
    match l["legal_hold"].as_bool() {
        Some(true) => parts.push("legal hold on".to_string()),
        Some(false) => parts.push("legal hold off".to_string()),
        None => {}
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// A Content-Type from the file extension.
fn guess_type(path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "txt" | "md" | "log" => "text/plain; charset=utf-8",
        "csv" => "text/csv",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

/// Where a download lands: into `dst` if it's a directory (or ends with `/`), else `dst` itself.
fn download_target(dst: &str, key: &str) -> PathBuf {
    let name = key.rsplit('/').next().unwrap_or(key);
    let p = PathBuf::from(dst);
    if dst.ends_with('/') || p.is_dir() { p.join(name) } else { p }
}

/// The key an upload of `file` goes to: `key` itself, or `key` + the file name when `key` is a
/// "directory" (empty or ending with `/`).
fn upload_key(key: &str, file: &Path) -> String {
    if key.is_empty() || key.ends_with('/') {
        let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        format!("{key}{name}")
    } else {
        key.to_string()
    }
}

/// Every file under `dir`, as `(path, key suffix with '/' separators)`, sorted.
fn walk(dir: &Path) -> anyhow::Result<Vec<(PathBuf, String)>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).with_context(|| format!("can't read {}", d.display()))? {
            let p = e?.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let rel = p.strip_prefix(dir)?.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
                out.push((p, rel));
            }
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(out)
}

impl Api {
    /// A request whose response isn't JSON (uploads and downloads); errors still are.
    async fn raw(&self, req: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
        let mut req = match &self.key {
            Some(k) => req.bearer_auth(k),
            None => req,
        };
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let res = req.send().await.with_context(|| format!("can't reach {} (is it running?)", self.base))?;
        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            let msg = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["error"].as_str().map(String::from));
            match msg {
                Some(m) => bail!("{m}"),
                None if text.trim().is_empty() => bail!("{status}"),
                None => bail!("{status}: {}", text.trim()),
            }
        }
        Ok(res)
    }
}

/// Upload `file`, or stdin when `None`. The API needs a Content-Length, so stdin is spooled to a
/// temporary file first.
async fn upload(api: &Api, bucket: &str, key: &str, file: Option<&Path>, content_type: Option<&str>) -> anyhow::Result<Value> {
    let (f, content_type, _spool) = match file {
        Some(p) => {
            let f = tokio::fs::File::open(p).await.with_context(|| format!("can't open {}", p.display()))?;
            (f, content_type.unwrap_or_else(|| guess_type(p)), None)
        }
        None => {
            let spool = tempfile::NamedTempFile::new().context("can't create a temporary file for stdin")?;
            let mut f = tokio::fs::File::create(spool.path()).await?;
            tokio::io::copy(&mut tokio::io::stdin(), &mut f).await.context("can't read stdin")?;
            f.flush().await?;
            (tokio::fs::File::open(spool.path()).await?, content_type.unwrap_or("application/octet-stream"), Some(spool))
        }
    };
    let len = f.metadata().await?.len();
    let url = format!("{}{}?key={}", api.base, bucket_path(bucket, "/upload"), enc(key));
    let req = api
        .http
        .put(url)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .header(reqwest::header::CONTENT_LENGTH, len)
        .body(reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(f)));
    Ok(api.raw(req).await?.json().await?)
}

/// Stream an object (or one of its versions) into `out` (a file path, or stdout for `-`).
/// Returns the bytes written.
async fn download(api: &Api, bucket: &str, key: &str, version_id: Option<&str>, out: Option<&Path>) -> anyhow::Result<u64> {
    let url = format!("{}{}{}", api.base, bucket_path(bucket, "/download"), key_query(key, version_id));
    let mut res = api.raw(api.http.get(url)).await?;
    let mut w: Box<dyn tokio::io::AsyncWrite + Unpin> = match out {
        Some(p) => {
            if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                tokio::fs::create_dir_all(dir).await?;
            }
            Box::new(tokio::fs::File::create(p).await.with_context(|| format!("can't create {}", p.display()))?)
        }
        None => Box::new(tokio::io::stdout()),
    };
    let mut n = 0u64;
    while let Some(chunk) = res.chunk().await? {
        w.write_all(&chunk).await?;
        n += chunk.len() as u64;
    }
    w.flush().await?;
    Ok(n)
}

/// Every page of a bucket listing.
async fn list_all(api: &Api, bucket: &str, prefix: &str, delimiter: bool) -> anyhow::Result<(Vec<Value>, Vec<String>)> {
    let (mut objects, mut prefixes) = (Vec::new(), Vec::new());
    let mut cursor: Option<String> = None;
    loop {
        let mut q = format!("?prefix={}&limit=1000", enc(prefix));
        if delimiter {
            q.push_str("&delimiter=%2F");
        }
        if let Some(c) = &cursor {
            q.push_str(&format!("&cursor={}", enc(c)));
        }
        let page = api.get(&bucket_path(bucket, &format!("/objects{q}"))).await?;
        objects.extend(page["objects"].as_array().into_iter().flatten().cloned());
        prefixes.extend(page["prefixes"].as_array().into_iter().flatten().filter_map(|p| p.as_str().map(String::from)));
        match page["next_cursor"].as_str() {
            Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
            _ => return Ok((objects, prefixes)),
        }
    }
}

/// Every page of a bucket's versions under `prefix`.
async fn list_versions(api: &Api, bucket: &str, prefix: &str) -> anyhow::Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut q = format!("?prefix={}&limit=1000", enc(prefix));
        if let Some(c) = &cursor {
            q.push_str(&format!("&cursor={}", enc(c)));
        }
        let page = api.get(&bucket_path(bucket, &format!("/versions{q}"))).await?;
        out.extend(page["versions"].as_array().into_iter().flatten().cloned());
        match page["next_cursor"].as_str() {
            Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
            _ => return Ok(out),
        }
    }
}

fn print_buckets(v: &Value) {
    let mut t = Table::new(&[("name", L), ("objects", R), ("size", R), ("access", L), ("recover", R), ("branch of", L), ("created", L)]);
    for b in v.as_array().into_iter().flatten() {
        t.row(vec![
            s(&b["name"]),
            count(f(&b["objects"])),
            size(f(&b["bytes"])),
            if b["public"].as_bool() == Some(true) { "public".into() } else { "private".into() },
            format!("{}d", s(&b["recover_days"])),
            b["parent"]["name"].as_str().unwrap_or("-").to_string(),
            when(&b["created_at"]),
        ]);
    }
    t.print();
}

fn print_bucket(b: &Value) {
    println!("{}  {}  {}", s(&b["name"]), s(&b["id"]), s(&b["region"]));
    println!("  objects    {}  ({})", count(f(&b["objects"])), size(f(&b["bytes"])));
    println!("  trash      {}  (recoverable {} days)", size(f(&b["trash_bytes"])), s(&b["recover_days"]));
    match b["versioning"].as_str() {
        Some(state) => {
            println!("  versioning {}", state.to_lowercase());
            println!("  versions   {} noncurrent  ({})", count(f(&b["versions"])), size(f(&b["version_bytes"])));
        }
        None => println!("  versioning off (never enabled)"),
    }
    if b["object_lock"].is_object() {
        match b["object_lock"]["mode"].as_str() {
            Some(m) => println!("  lock       on, new versions kept {} ({m})", period(&b["object_lock"])),
            None => println!("  lock       on, no default retention"),
        }
    }
    println!("  access     {}", if b["public"].as_bool() == Some(true) { "public (anonymous GET)" } else { "private" });
    if let Some(p) = b["parent"]["name"].as_str() {
        println!("  branch of  {p} at {}", when(&b["parent"]["at"]));
    }
    let branches: Vec<String> = b["branches"].as_array().into_iter().flatten().map(s).collect();
    if !branches.is_empty() {
        println!("  branches   {}", branches.join(", "));
    }
    for r in b["cors"].as_array().into_iter().flatten() {
        let list = |v: &Value| v.as_array().into_iter().flatten().map(s).collect::<Vec<_>>().join(",");
        println!("  cors       {} → {}", list(&r["allowed_origins"]), list(&r["allowed_methods"]));
    }
    for r in b["lifecycle"].as_array().into_iter().flatten() {
        println!("  lifecycle  {}", lifecycle_line(r));
    }
    for (k, v) in b["tags"].as_object().into_iter().flatten() {
        println!("  tag        {k} = {}", s(v));
    }
    if let Some(w) = b["website"].as_object() {
        match w.get("redirect_all").filter(|r| !r.is_null()) {
            Some(r) => {
                let proto = r["protocol"].as_str().map(|p| format!("{p}://")).unwrap_or_default();
                println!("  website    redirects to {proto}{}", s(&r["host_name"]));
            }
            None => {
                let mut line = format!("index {}", w.get("index_document").map(s).unwrap_or_default());
                if let Some(e) = w.get("error_document").and_then(Value::as_str) {
                    line.push_str(&format!(", error {e}"));
                }
                let rules = w.get("routing_rules").and_then(Value::as_array).map_or(0, Vec::len);
                if rules > 0 {
                    line.push_str(&format!(", {rules} routing rule{}", if rules == 1 { "" } else { "s" }));
                }
                println!("  website    {line}");
            }
        }
    }
    if let Some(u) = b["website_url"].as_str() {
        println!("             {u}");
    }
    if !b["policy"].is_null() {
        println!("  policy     {}", if b["policy_public"].as_bool() == Some(true) { "set, public" } else { "set" });
    }
    if let Some(p) = b["public_access_block"].as_object() {
        let on: Vec<&str> = [("block_public_acls", "BlockPublicAcls"), ("ignore_public_acls", "IgnorePublicAcls"), ("block_public_policy", "BlockPublicPolicy"), ("restrict_public_buckets", "RestrictPublicBuckets")]
            .into_iter()
            .filter(|(k, _)| p.get(*k).and_then(Value::as_bool) == Some(true))
            .map(|(_, n)| n)
            .collect();
        println!("  blocked    {}", if on.is_empty() { "nothing".to_string() } else { on.join(", ") });
    }
    let n = &b["notifications"];
    for c in n["configurations"].as_array().into_iter().flatten() {
        println!("  notify     {}", events::notify_line(c, &n["backlog"]));
    }
    if n["event_bridge"].as_bool() == Some(true) {
        println!("  notify     every event → EventBridge");
    }
    for r in b["replication"]["rules"].as_array().into_iter().flatten() {
        println!("  replicate  {}", events::replicate_line(r));
    }
    if b["cdn"]["enabled"].as_bool() == Some(true) {
        println!("  cdn        {} ({})", s(&b["public_url"]), s(&b["cdn"]["mode"]));
    }
    println!("  url        {}", s(&b["url"]));
    if let Some(v) = b["virtual_host_url"].as_str() {
        println!("             {v}");
    }
}

fn print_usage_row(t: &mut Table, name: String, u: &Value) {
    let c = &u["cost"];
    t.row(vec![
        name,
        size(f(&u["storage_bytes"])),
        format!("{:.3}", f(&u["gb_months"])),
        count(f(&u["class_a"])),
        count(f(&u["class_b"])),
        size(f(&u["egress_internet_bytes"])),
        size(f(&u["egress_internal_bytes"])),
        size(f(&u["egress_cached_bytes"])),
        usd(f(&c["total_usd"])),
        usd(f(&c["projected_month_usd"])),
    ]);
}

fn usage_table() -> Table {
    Table::new(&[
        ("bucket", L),
        ("stored", R),
        ("gb-months", R),
        ("class a", R),
        ("class b", R),
        ("egress", R),
        ("internal", R),
        ("cdn", R),
        ("cost", R),
        ("/month", R),
    ])
}

pub async fn run(api: &Api, as_json: bool, cmd: Storage) -> anyhow::Result<()> {
    // The account `roll storage accounts use` chose (accounts themselves are the deployment's).
    let scoped;
    let api = match accounts::current() {
        Some(a) if !matches!(cmd, Storage::Accounts { .. }) => {
            scoped = Api { base: api.base.clone(), http: api.http.clone(), key: api.key.clone(), headers: vec![("x-roll-account".into(), a)] };
            &scoped
        }
        _ => api,
    };
    let out = match cmd {
        Storage::Accounts { action } => match accounts::accounts(api, as_json, action).await? {
            Some(v) => v,
            None => return Ok(()),
        },
        Storage::Iam { action } => match iam::iam(api, as_json, action).await? {
            Some(v) => v,
            None => return Ok(()),
        },
        Storage::Info => {
            let v = api.get("/v1/storage").await?;
            if !as_json {
                let p = &v["pricing"];
                println!("endpoint      {}", s(&v["endpoint"]));
                if let Some(d) = v["virtual_host_domain"].as_str() {
                    println!("virtual host  <bucket>.{d}");
                }
                println!("region        {}", s(&v["region"]));
                println!("storage       {} ({})", s(&v["storage"]), s(&v["storage_class"]));
                println!("buckets       {}  objects {}  {}", count(f(&v["buckets"])), count(f(&v["objects"])), size(f(&v["bytes"])));
                println!("recover       {} days by default", s(&v["recover_days_default"]));
                println!(
                    "pricing       {}/GB-month  class A {}/M  class B {}/M  egress {}/GB (internal {}, through the CDN {})",
                    price(f(&p["usd_per_gb_month"])),
                    price(f(&p["usd_per_million_class_a"])),
                    price(f(&p["usd_per_million_class_b"])),
                    price(f(&p["usd_per_gb_egress_internet"])),
                    price(f(&p["usd_per_gb_egress_internal"])),
                    price(f(&p["usd_per_gb_egress_cached"])),
                );
                if let Some(c) = v["cache"].as_object().filter(|c| c.get("enabled").and_then(Value::as_bool) == Some(true)) {
                    let n = |k: &str| c.get(k).and_then(Value::as_f64).unwrap_or(0.0);
                    println!("cache         {} hits, {} misses, {} S3 GETs avoided, {} saved", count(n("memory_hits") + n("disk_hits")), count(n("misses")), count(n("gets_avoided")), size(n("bytes_saved")));
                }
                let sc = &v["scrub"];
                if sc["missing"].as_i64().unwrap_or(0) > 0 || sc["mismatched"].as_i64().unwrap_or(0) > 0 {
                    eprintln!("scrub: {} missing and {} mismatched blobs (last run {})", s(&sc["missing"]), s(&sc["mismatched"]), when(&sc["last_at"]));
                }
                return Ok(());
            }
            v
        }
        Storage::Ls { path: None, versions: true, .. } => bail!("--versions lists a bucket: give <bucket>[/prefix]"),
        Storage::Buckets | Storage::Ls { path: None, .. } => {
            let v = api.get("/v1/storage/buckets").await?;
            if !as_json {
                if v.as_array().is_none_or(|a| a.is_empty()) {
                    println!("no buckets; create one with `roll storage create <name>`");
                } else {
                    print_buckets(&v);
                }
                return Ok(());
            }
            v
        }
        Storage::Create { name, public, recover_days, object_lock } => {
            let mut body = json!({ "name": name, "public": public, "recover_days": recover_days });
            if object_lock {
                body["object_lock"] = json!(true);
            }
            let v = api.post("/v1/storage/buckets", body).await?;
            if !as_json {
                println!("created bucket {}\n{}", s(&v["name"]), s(&v["url"]));
                return Ok(());
            }
            v
        }
        Storage::Show { bucket } => {
            let v = api.get(&bucket_path(&bucket, "")).await?;
            if !as_json {
                print_bucket(&v);
                return Ok(());
            }
            v
        }
        Storage::Url { path, immutable, expires } => {
            let (bucket, key) = split_key(&path)?;
            let mut body = json!({ "key": key });
            if immutable {
                body["immutable"] = json!(true);
            }
            if let Some(e) = expires {
                body["expires_s"] = json!(duration(&e)?);
            }
            let v = api.post(&bucket_path(&bucket, "/url"), body).await?;
            if !as_json {
                println!("{}", s(&v["url"]));
                let via = if v["cdn"].as_bool() == Some(true) { if v["immutable"].as_bool() == Some(true) { "CDN, immutable" } else { "CDN" } } else { "S3" };
                match v["expires_at"].as_i64() {
                    Some(_) => eprintln!("{via}, valid until {} UTC", when(&v["expires_at"])),
                    None => eprintln!("{via}"),
                }
                return Ok(());
            }
            v
        }
        Storage::Cdn { base_url, zone_id, api_token, purge, mutable_ttl, worker } => {
            if worker {
                let v = api.get("/v1/storage/cdn/worker?secrets=true").await?;
                if !as_json {
                    for (k, val) in v["vars"].as_object().into_iter().flatten() {
                        println!("{k}={}", val.as_str().map(str::to_string).unwrap_or_else(|| val.to_string()));
                    }
                    for n in v["secrets"].as_array().into_iter().flatten() {
                        eprintln!("secret: wrangler secret put {}", s(n));
                    }
                    return Ok(());
                }
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            let mut body = json!({});
            for (k, val) in [("base_url", base_url), ("zone_id", zone_id), ("api_token", api_token), ("purge", purge)] {
                if let Some(val) = val {
                    body[k] = json!(val);
                }
            }
            if let Some(t) = mutable_ttl {
                body["mutable_ttl_s"] = json!(t);
            }
            let v = if body.as_object().is_some_and(|o| o.is_empty()) { api.get("/v1/storage/cdn").await? } else { api.call(Method::PATCH, "/v1/storage/cdn", Some(body)).await? };
            if !as_json {
                println!("base url      {}", v["base_url"].as_str().unwrap_or("(none: no CDN)"));
                println!("zone          {}", v["zone_id"].as_str().unwrap_or("-"));
                println!("api token     {}", if v["api_token_set"].as_bool() == Some(true) { "set" } else { "not set (purges wait)" });
                println!("purge         by {}  ttl {}s", s(&v["purge"]), s(&v["mutable_ttl_s"]));
                let m = &v["margin"];
                if m.is_object() {
                    println!(
                        "last 30 days  {} served by the CDN, {} pulled from roll (byte hit ratio {:.1}%), billed {}",
                        size(f(&m["cached_bytes"])),
                        size(f(&m["origin_pull_bytes"])),
                        f(&m["byte_hit_ratio"]) * 100.0,
                        usd(f(&m["billed_usd"])),
                    );
                }
                let c = &v["cache"];
                if c["enabled"].as_bool() == Some(true) {
                    println!(
                        "gateway cache {} hits ({} from disk), {} misses, {} S3 GETs avoided, {} saved",
                        count(f(&c["memory_hits"]) + f(&c["disk_hits"])),
                        count(f(&c["disk_hits"])),
                        count(f(&c["misses"])),
                        count(f(&c["gets_avoided"])),
                        size(f(&c["bytes_saved"])),
                    );
                }
                return Ok(());
            }
            v
        }
        Storage::Update { bucket, public, private, recover_days, cors_origins, no_cors, tags, no_tags, versioning, cdn } => {
            let mut body = json!({});
            if public || private {
                body["public"] = json!(public);
            }
            if let Some(d) = recover_days {
                body["recover_days"] = json!(d);
            }
            if no_cors {
                body["cors"] = json!([]);
            } else if !cors_origins.is_empty() {
                body["cors"] = json!([{
                    "allowed_origins": cors_origins,
                    "allowed_methods": ["GET", "HEAD", "PUT", "POST", "DELETE"],
                    "allowed_headers": ["*"],
                    "expose_headers": ["ETag"],
                    "max_age_s": 3600,
                }]);
            }
            if no_tags {
                body["tags"] = json!({});
            } else if !tags.is_empty() {
                let mut map = serde_json::Map::new();
                for t in &tags {
                    let (k, v) = t.split_once('=').with_context(|| format!("--tag {t:?}: expected key=value"))?;
                    map.insert(k.to_string(), json!(v));
                }
                body["tags"] = Value::Object(map);
            }
            match versioning {
                Some(Toggle::On) => body["versioning"] = json!("Enabled"),
                Some(Toggle::Off) => body["versioning"] = json!("Suspended"),
                None => {}
            }
            match cdn {
                Some(CdnArg::Off) => body["cdn"] = Value::Null,
                Some(CdnArg::Mutable) => body["cdn"] = json!({ "enabled": true, "mode": "mutable" }),
                Some(CdnArg::Immutable) => body["cdn"] = json!({ "enabled": true, "mode": "immutable" }),
                None => {}
            }
            if body.as_object().is_some_and(|o| o.is_empty()) {
                bail!("nothing to change: pass --public, --private, --recover-days, --cors-origin, --no-cors, --tag, --no-tags, --versioning or --cdn");
            }
            let v = api.call(Method::PATCH, &bucket_path(&bucket, ""), Some(body)).await?;
            if !as_json {
                print_bucket(&v);
                return Ok(());
            }
            v
        }
        Storage::Delete { bucket, force } => {
            let q = if force { "?force=true" } else { "" };
            let v = api.call(Method::DELETE, &bucket_path(&bucket, q), None).await?;
            if !as_json {
                println!("deleted bucket {bucket}");
                return Ok(());
            }
            v
        }
        Storage::Branch { bucket, name } => {
            let v = api.post(&bucket_path(&bucket, "/branches"), json!({ "name": name })).await?;
            if !as_json {
                println!("branch {} of {bucket} (copy-on-write, nothing copied)\n{}", s(&v["name"]), s(&v["url"]));
                return Ok(());
            }
            v
        }
        Storage::Ls { path: Some(path), versions: true, .. } => {
            let (bucket, prefix) = split(&path)?;
            let versions = list_versions(api, &bucket, &prefix).await?;
            if as_json {
                json!({ "versions": versions })
            } else {
                if versions.is_empty() {
                    println!("no versions under {bucket}/{prefix}");
                    return Ok(());
                }
                let mut t = Table::new(&[("modified", L), ("size", R), ("", L), ("version id", L), ("key", L)]);
                for v in &versions {
                    let marker = v["delete_marker"].as_bool() == Some(true);
                    let flag = match (v["is_latest"].as_bool() == Some(true), marker) {
                        (true, true) => "latest, delete marker",
                        (true, false) => "latest",
                        (false, true) => "delete marker",
                        (false, false) => "",
                    };
                    let sz = if marker { "-".to_string() } else { size(f(&v["size"])) };
                    t.row(vec![when(&v["last_modified"]), sz, flag.to_string(), s(&v["version_id"]), s(&v["key"])]);
                }
                t.print();
                return Ok(());
            }
        }
        Storage::Ls { path: Some(path), recursive, .. } => {
            let (bucket, prefix) = split(&path)?;
            let (objects, prefixes) = list_all(api, &bucket, &prefix, !recursive).await?;
            if as_json {
                json!({ "objects": objects, "prefixes": prefixes })
            } else {
                let mut t = Table::new(&[("", L), ("", R), ("", L)]);
                for p in &prefixes {
                    t.row(vec![String::new(), "DIR".into(), p.clone()]);
                }
                for o in &objects {
                    t.row(vec![when(&o["last_modified"]), size(f(&o["size"])), s(&o["key"])]);
                }
                t.print();
                if objects.is_empty() && prefixes.is_empty() {
                    println!("no objects under {bucket}/{prefix}");
                }
                return Ok(());
            }
        }
        Storage::Cp { src, dst, recursive, content_type, version_id } => {
            let remote_src = src.starts_with("s3://") || (src != "-" && !Path::new(&src).exists());
            if !remote_src && version_id.is_some() {
                bail!("--version-id is for downloads; an upload always makes a new version");
            }
            if !remote_src {
                let (bucket, key) = split(&dst)?;
                if recursive {
                    let dir = Path::new(&src);
                    if !dir.is_dir() {
                        bail!("{src} is not a directory");
                    }
                    let prefix = if key.is_empty() || key.ends_with('/') { key } else { format!("{key}/") };
                    let files = walk(dir)?;
                    let mut total = 0u64;
                    for (p, rel) in &files {
                        let v = upload(api, &bucket, &format!("{prefix}{rel}"), Some(p), content_type.as_deref()).await?;
                        total += v["size"].as_u64().unwrap_or(0);
                        if !as_json {
                            println!("{} → {bucket}/{}", p.display(), s(&v["key"]));
                        }
                    }
                    if !as_json {
                        println!("uploaded {} files ({})", files.len(), size(total as f64));
                        return Ok(());
                    }
                    json!({ "uploaded": files.len(), "bytes": total })
                } else if src == "-" {
                    if key.is_empty() || key.ends_with('/') {
                        bail!("give the object a key when uploading from stdin");
                    }
                    let v = upload(api, &bucket, &key, None, content_type.as_deref()).await?;
                    if !as_json {
                        println!("stdin → {bucket}/{}  {}", s(&v["key"]), size(f(&v["size"])));
                        return Ok(());
                    }
                    v
                } else {
                    let p = Path::new(&src);
                    if p.is_dir() {
                        bail!("{src} is a directory; use -r");
                    }
                    let v = upload(api, &bucket, &upload_key(&key, p), Some(p), content_type.as_deref()).await?;
                    if !as_json {
                        println!("{src} → {bucket}/{}  {}", s(&v["key"]), size(f(&v["size"])));
                        return Ok(());
                    }
                    v
                }
            } else {
                let (bucket, key) = split(&src)?;
                if recursive {
                    if dst == "-" {
                        bail!("can't download several objects to stdout");
                    }
                    let (objects, _) = list_all(api, &bucket, &key, false).await?;
                    let mut total = 0u64;
                    for o in &objects {
                        let k = s(&o["key"]);
                        let rel = k[key.len()..].trim_start_matches('/');
                        let target = Path::new(&dst).join(rel);
                        total += download(api, &bucket, &k, None, Some(&target)).await?;
                        if !as_json {
                            println!("{bucket}/{k} → {}", target.display());
                        }
                    }
                    if !as_json {
                        println!("downloaded {} objects ({})", objects.len(), size(total as f64));
                        return Ok(());
                    }
                    json!({ "downloaded": objects.len(), "bytes": total })
                } else {
                    if key.is_empty() {
                        bail!("expected <bucket>/<key>, got {src:?} (use -r for a prefix)");
                    }
                    let vid = version_id.as_deref();
                    if dst == "-" {
                        download(api, &bucket, &key, vid, None).await?;
                        return Ok(());
                    }
                    let target = download_target(&dst, &key);
                    let n = download(api, &bucket, &key, vid, Some(&target)).await?;
                    if !as_json {
                        let at = vid.map(|v| format!(" (version {v})")).unwrap_or_default();
                        println!("{bucket}/{key}{at} → {}  {}", target.display(), size(n as f64));
                        return Ok(());
                    }
                    let mut v = json!({ "key": key, "path": target, "bytes": n });
                    if let Some(id) = vid {
                        v["version_id"] = json!(id);
                    }
                    v
                }
            }
        }
        Storage::Rm { path, recursive: false, version_id: Some(vid) } => {
            let (bucket, key) = split_key(&path)?;
            let v = api.call(Method::DELETE, &format!("{}{}", bucket_path(&bucket, "/object"), key_query(&key, Some(&vid))), None).await?;
            if !as_json {
                let what = if v["delete_marker"].as_bool() == Some(true) { "delete marker" } else { "version" };
                println!("deleted {what} {vid} of {bucket}/{key} for good (recoverable from the trash)");
                return Ok(());
            }
            v
        }
        Storage::Rm { path, recursive, .. } => {
            if recursive {
                let (bucket, prefix) = split(&path)?;
                let v = api.post(&bucket_path(&bucket, "/delete"), json!({ "prefix": prefix })).await?;
                if !as_json {
                    println!("deleted {} objects under {bucket}/{prefix} (recoverable from the trash)", count(f(&v["deleted"])));
                    return Ok(());
                }
                v
            } else {
                let (bucket, key) = split_key(&path)?;
                let v = api.call(Method::DELETE, &format!("{}?key={}", bucket_path(&bucket, "/object"), enc(&key)), None).await?;
                if !as_json {
                    match v["version_id"].as_str() {
                        Some(m) if v["delete_marker"].as_bool() == Some(true) => println!("deleted {bucket}/{key} (delete marker {m}; older versions are kept)"),
                        _ => println!("deleted {bucket}/{key}"),
                    }
                    return Ok(());
                }
                v
            }
        }
        Storage::Stat { path, version_id } => {
            let (bucket, key) = split_key(&path)?;
            let v = api.get(&format!("{}{}", bucket_path(&bucket, "/object"), key_query(&key, version_id.as_deref()))).await?;
            if !as_json {
                println!("{bucket}/{}", s(&v["key"]));
                println!("  version        {}", s(&v["version_id"]));
                if let Some(l) = lock_line(&v["lock"]) {
                    println!("  lock           {l}");
                }
                println!("  size           {} ({} bytes)", size(f(&v["size"])), s(&v["size"]));
                println!("  etag           {}", s(&v["etag"]));
                println!("  content-type   {}", s(&v["content_type"]));
                println!("  last modified  {} UTC", when(&v["last_modified"]));
                for (k, label) in [
                    ("cache_control", "cache-control"),
                    ("content_disposition", "disposition"),
                    ("content_encoding", "encoding"),
                    ("content_language", "language"),
                    ("expires", "expires"),
                ] {
                    if let Some(x) = v[k].as_str() {
                        println!("  {label:<14} {x}");
                    }
                }
                if let Some(c) = v["checksum"].as_object() {
                    println!("  checksum       {} {} ({})", s(&c["algorithm"]), s(&c["value"]), s(&c["type"]));
                }
                if v["parts"].as_i64().unwrap_or(0) > 0 {
                    println!("  parts          {}", s(&v["parts"]));
                }
                for (field, label) in [("metadata", "meta"), ("tags", "tag")] {
                    for (k, x) in v[field].as_object().into_iter().flatten() {
                        println!("  {label:<5} {k} = {}", s(x));
                    }
                }
                return Ok(());
            }
            v
        }
        Storage::Presign { path, put, expires } => {
            let (bucket, key) = split_key(&path)?;
            let secs = duration(&expires)?;
            if !(1..=7 * 86_400).contains(&secs) {
                bail!("--expires must be between 1s and 7d");
            }
            let method = if put { "PUT" } else { "GET" };
            let v = api.post(&bucket_path(&bucket, "/presign"), json!({ "key": key, "method": method, "expires_s": secs })).await?;
            if !as_json {
                println!("{}", s(&v["url"]));
                eprintln!("{method}, valid until {} UTC", when(&v["expires_at"]));
                return Ok(());
            }
            v
        }
        Storage::Trash { bucket, prefix } => {
            let mut entries = Vec::new();
            let mut cursor: Option<String> = None;
            loop {
                let mut q = format!("?limit=1000&prefix={}", enc(prefix.as_deref().unwrap_or("")));
                if let Some(c) = &cursor {
                    q.push_str(&format!("&cursor={}", enc(c)));
                }
                let page = api.get(&bucket_path(&bucket, &format!("/trash{q}"))).await?;
                entries.extend(page["objects"].as_array().into_iter().flatten().cloned());
                match page["next_cursor"].as_str() {
                    Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                    _ => break,
                }
            }
            if !as_json {
                if entries.is_empty() {
                    println!("the trash of {bucket} is empty");
                    return Ok(());
                }
                let mut t = Table::new(&[("deleted_at", R), ("deleted", L), ("size", R), ("purged", L), ("key", L)]);
                for e in &entries {
                    t.row(vec![s(&e["deleted_at"]), when(&e["deleted_at"]), size(f(&e["size"])), when(&e["expires_at"]), s(&e["key"])]);
                }
                t.print();
                println!("restore with `roll storage restore {bucket}/<key> --at <deleted_at>`");
                return Ok(());
            }
            json!(entries)
        }
        Storage::Restore { path, at } => {
            let (bucket, key) = split_key(&path)?;
            let v = api.post(&bucket_path(&bucket, "/trash/restore"), json!({ "key": key, "deleted_at": at })).await?;
            if !as_json {
                println!("restored {bucket}/{}  {}", s(&v["key"]), size(f(&v["size"])));
                return Ok(());
            }
            v
        }
        Storage::Usage { bucket: Some(bucket), days } => {
            let u = api.get(&bucket_path(&bucket, &format!("/usage?days={days}"))).await?;
            if !as_json {
                let c = &u["cost"];
                println!("{bucket} — last {days} days");
                println!("  storage   {:>10}  {:>10.3} GB-months  {}", size(f(&u["storage_bytes"])), f(&u["gb_months"]), usd(f(&c["storage_usd"])));
                println!("  class A   {:>10}  {:>21}", count(f(&u["class_a"])), usd(f(&c["class_a_usd"])));
                println!("  class B   {:>10}  {:>21}", count(f(&u["class_b"])), usd(f(&c["class_b_usd"])));
                println!("  egress    {:>10}  {:>21}", size(f(&u["egress_internet_bytes"])), usd(f(&c["egress_usd"])));
                println!("  internal  {:>10}  {:>21}", size(f(&u["egress_internal_bytes"])), "free");
                println!("  cdn       {:>10}  {:>21}", size(f(&u["egress_cached_bytes"])), usd(f(&c["egress_cached_usd"])));
                println!("  uploaded  {:>10}  {:>21}", size(f(&u["bytes_in"])), "free");
                println!("  total                 {:>21}", usd(f(&c["total_usd"])));
                println!("  projected / month     {:>21}", usd(f(&c["projected_month_usd"])));
                return Ok(());
            }
            u
        }
        Storage::Usage { bucket: None, days } => {
            let u = api.get(&format!("/v1/storage/usage?days={days}")).await?;
            if !as_json {
                println!("last {days} days");
                let mut t = usage_table();
                for b in u["buckets"].as_array().into_iter().flatten() {
                    print_usage_row(&mut t, s(&b["bucket"]), b);
                }
                t.print();
                let c = &u["cost"];
                println!(
                    "total {}  (storage {}, class A {}, class B {}, egress {}, cdn {})  projected {} / month",
                    usd(f(&c["total_usd"])),
                    usd(f(&c["storage_usd"])),
                    usd(f(&c["class_a_usd"])),
                    usd(f(&c["class_b_usd"])),
                    usd(f(&c["egress_usd"])),
                    usd(f(&c["egress_cached_usd"])),
                    usd(f(&c["projected_month_usd"])),
                );
                return Ok(());
            }
            u
        }
        Storage::Keys { action: None } => {
            let v = api.get("/v1/storage/keys").await?;
            if !as_json {
                let mut t = Table::new(&[("access key id", L), ("name", L), ("permission", L), ("buckets", L), ("created", L), ("last used", L)]);
                for k in v.as_array().into_iter().flatten() {
                    let buckets = match k["buckets"].as_array() {
                        Some(b) => b.iter().map(s).collect::<Vec<_>>().join(","),
                        None => "all".into(),
                    };
                    t.row(vec![s(&k["id"]), s(&k["name"]), s(&k["permission"]), buckets, when(&k["created_at"]), when(&k["last_used_at"])]);
                }
                t.print();
                return Ok(());
            }
            v
        }
        Storage::Keys { action: Some(KeysCmd::Create { name, permission, buckets }) } => {
            let buckets = if buckets.is_empty() { Value::Null } else { json!(buckets) };
            let v = api.post("/v1/storage/keys", json!({ "name": name, "permission": permission.as_str(), "buckets": buckets })).await?;
            if !as_json {
                let info = api.get("/v1/storage").await?;
                println!("created {} key {} ({})", s(&v["permission"]), s(&v["id"]), s(&v["name"]));
                eprintln!("the secret is shown only once: store it now");
                println!();
                println!("AWS_ACCESS_KEY_ID={}", s(&v["id"]));
                println!("AWS_SECRET_ACCESS_KEY={}", s(&v["secret_access_key"]));
                println!("AWS_ENDPOINT_URL={}", s(&info["endpoint"]));
                println!("AWS_REGION={}", s(&info["region"]));
                return Ok(());
            }
            v
        }
        Storage::Roles { action } => match roles::roles(api, as_json, action).await? {
            Some(v) => v,
            None => return Ok(()),
        },
        Storage::Sessions { action } => match roles::sessions(api, as_json, action).await? {
            Some(v) => v,
            None => return Ok(()),
        },
        Storage::Notify { bucket, action } => match events::notify(api, as_json, bucket, action).await? {
            Some(v) => v,
            None => return Ok(()),
        },
        Storage::Replicate { bucket, to, prefix, delete_markers, existing, id, action } => {
            match events::replicate(api, as_json, bucket, to, prefix, delete_markers, existing, id, action).await? {
                Some(v) => v,
                None => return Ok(()),
            }
        }
        Storage::Keys { action: Some(KeysCmd::Delete { id }) } => {
            let v = api.call(Method::DELETE, &format!("/v1/storage/keys/{}", enc(&id)), None).await?;
            if !as_json {
                println!("deleted key {id}");
                return Ok(());
            }
            v
        }
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(split("assets/img/a.png").unwrap(), ("assets".into(), "img/a.png".into()));
        assert_eq!(split("s3://assets/img/a.png").unwrap(), ("assets".into(), "img/a.png".into()));
        assert_eq!(split("assets").unwrap(), ("assets".into(), "".into()));
        assert_eq!(split("s3://assets/").unwrap(), ("assets".into(), "".into()));
        assert_eq!(split("assets/img/").unwrap(), ("assets".into(), "img/".into()));
        // Keys keep everything after the first slash, including doubled slashes.
        assert_eq!(split("b//x").unwrap(), ("b".into(), "/x".into()));
        assert!(split("").is_err());
        assert!(split("/key").is_err());
        assert!(split("s3:///key").is_err());
        assert!(split_key("assets").is_err());
        assert!(split_key("s3://assets/").is_err());
        assert_eq!(split_key("s3://a/b").unwrap(), ("a".into(), "b".into()));
    }

    #[test]
    fn upload_keys() {
        let f = Path::new("/tmp/dir/photo.png");
        assert_eq!(upload_key("", f), "photo.png");
        assert_eq!(upload_key("img/", f), "img/photo.png");
        assert_eq!(upload_key("img/cat.png", f), "img/cat.png");
        assert_eq!(download_target("out/", "img/cat.png"), PathBuf::from("out/cat.png"));
        assert_eq!(download_target("cat-copy.png", "img/cat.png"), PathBuf::from("cat-copy.png"));
    }

    #[test]
    fn sizes() {
        assert_eq!(size(0.0), "0 B");
        assert_eq!(size(512.0), "512 B");
        assert_eq!(size(1536.0), "1.5 KB");
        assert_eq!(size(5.0 * 1024.0 * 1024.0), "5.0 MB");
        assert_eq!(size(3.25 * 1024f64.powi(4)), "3.2 TB");
    }

    #[test]
    fn durations_and_times() {
        assert_eq!(duration("90").unwrap(), 90);
        assert_eq!(duration("15m").unwrap(), 900);
        assert_eq!(duration("1h").unwrap(), 3600);
        assert_eq!(duration("7d").unwrap(), 604_800);
        assert!(duration("1w").is_err());
        assert!(duration("h").is_err());
        assert!(duration("-5m").is_err());
        assert_eq!(duration("999999999999999d").unwrap(), i64::MAX);
        assert_eq!(when(&json!(1_760_968_980_000i64)), "2025-10-20 14:03");
        assert_eq!(when(&Value::Null), "-");
    }

    #[test]
    fn content_types() {
        assert_eq!(guess_type(Path::new("a/b.PNG")), "image/png");
        assert_eq!(guess_type(Path::new("index.html")), "text/html; charset=utf-8");
        assert_eq!(guess_type(Path::new("noext")), "application/octet-stream");
    }
}
