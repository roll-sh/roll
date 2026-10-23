# roll storage: developer API

The developer API is a REST API compatible with Supabase Storage. It runs on its own listener over
the same buckets and objects as the S3 API, so a bucket is reachable both ways at once.
`@supabase/storage-js` works against it unchanged. roll's own features (versions, the trash,
branches, bucket policies) are under `/roll/...`, and `@roll/storage` wraps them.

- **Rules are bucket policies.** The S3 API's own evaluator decides every request; the caller's
  JWT claims are condition keys (`${jwt:sub}`). There's no row-level security.
- **Writes take the S3 API's write path.** One object is one blob, acknowledged once its
  metadata commit is durable. Bucket limits are enforced on every write path, the S3 API
  included.
- **Deletes go to the trash** and stay restorable for the bucket's `recover_days`.

The code is in `crates/s3-api` (routes), `crates/s3-server/src/app.rs` (callers, the evaluator,
presets) and `crates/s3-store/src/api.rs` (limits, projects and keys, resumable uploads).

## Endpoint

```text
http://<host>:59001/storage/v1
```

| setting | default | flag / env |
|---|---|---|
| listen address | `127.0.0.1:59001` | `--storage-api` / `ROLL_STORAGE_API` |
| public URL | `http://<public host>:<port>/storage/v1` | `--storage-api-public-url` / `ROLL_STORAGE_API_PUBLIC_URL` |

- The public URL is used for the upload URLs that resumable uploads hand out (`Location`) and is
  what the dashboard shows. Without it, `Location` is built from the request's `Host`.
  `X-Forwarded-Host` and `X-Forwarded-Proto` are only honoured from peers inside roll
  (`ROLL_S3_INTERNAL_CIDRS`).
- Every route also works without the `/storage/v1` prefix.
- CORS is open, as on Supabase: any origin and every method. Preflights are cached for 24 h.
- Uploads of unknown length are spooled to disk, and resumable uploads buffer their next part
  there: `<ROLL_HOME>/storage/spool` (`tus/` for resumable uploads).
- A standard upload (one request) can be up to 5 GiB and a resumable upload up to 5 TiB.

Each account is one **project**, with an anon key and a service key. You can get them in the
dashboard (Storage → API) or from `GET /v1/storage/api` (see [Control plane](#control-plane-and-dashboard)).

You can also run a standalone server with a store in a local directory, the S3 API and the
developer API (the `packages/storage` tests use it). It prints one line of JSON with the URLs
and keys:

```sh
cargo run -p s3-api --example storage-api -- --dir /tmp/roll-api [--port 0] [--s3-port 0] [--jwt-secret <secret>]
# {"url": "http://127.0.0.1:<port>/storage/v1", "s3": "http://127.0.0.1:<port>", "account": "…",
#  "anon_key": "…", "service_key": "…", "jwt_secret": "…"}
```

Its default `--jwt-secret` is Supabase's local development secret.

## Clients

### `@roll/storage`

This is storage-js pointed at roll, plus roll's extras under `.roll`. It re-exports everything
from `@supabase/storage-js`.

```ts
import { createClient } from "@roll/storage";

const storage = createClient("http://127.0.0.1:59001/storage/v1", ANON_KEY, {
  // A user's JWT (or a function returning one, resolved per request). Defaults to the key.
  accessToken: async () => (await auth.getSession()).data.session?.access_token ?? null,
});

await storage.from("avatars").upload(`${userId}/avatar.png`, file, { contentType: "image/png", upsert: true });
const { data } = await storage.from("avatars").createSignedUrl(`${userId}/avatar.png`, 3600);

// roll's extras: all return { data, error }, like storage-js
await storage.roll.versions("avatars", { prefix: `${userId}/`, limit: 100, cursor: null });
await storage.roll.trash("avatars");
await storage.roll.restore("avatars", `${userId}/avatar.png`, item.deleted_at);
await storage.roll.branches("avatars");
await storage.roll.createBranch("avatars", "avatars-preview");   // service key
await storage.roll.getPolicy("avatars");                         // service key
await storage.roll.setPolicy("avatars", policyDocument);         // service key
await storage.roll.deletePolicy("avatars");                      // service key
await storage.roll.applyRules("avatars", "user-folders");        // service key
```

`createClient(url, key)` sends `apikey: <key>` and `Authorization: Bearer <accessToken ?? key>`,
the same headers supabase-js sends.

### `@supabase/storage-js`

```ts
import { StorageClient } from "@supabase/storage-js";

const storage = new StorageClient("http://127.0.0.1:59001/storage/v1", {
  apikey: ANON_KEY,
  Authorization: `Bearer ${userJwt ?? ANON_KEY}`,
});
```

### supabase-js

`createClient("http://<host>:59001", ROLL_ANON_KEY)` works for storage only, because supabase-js
appends `/storage/v1` itself. supabase-js has no separate storage URL, so if your app also uses
Supabase Auth or the database, keep supabase-js for those. Use `@roll/storage` for storage, with
the session's access token:

```ts
const supabase = createSupabaseClient(SUPABASE_URL, SUPABASE_ANON_KEY);
const storage = createClient(ROLL_STORAGE_URL, ROLL_ANON_KEY, {
  accessToken: async () => (await supabase.auth.getSession()).data.session?.access_token ?? null,
});
```

The `apikey` must be one of roll's keys. Supabase's anon key doesn't verify at roll.

### Resumable uploads (`tus-js-client`)

```ts
import * as tus from "tus-js-client";

new tus.Upload(file, {
  endpoint: `${ROLL_STORAGE_URL}/upload/resumable`,
  headers: { apikey: ANON_KEY, authorization: `Bearer ${accessToken}`, "x-upsert": "true" },
  metadata: { bucketName: "videos", objectName: `${userId}/clip.mp4`, contentType: "video/mp4", cacheControl: "3600" },
  chunkSize: 6 * 1024 * 1024, // any size works; Supabase requires exactly 6 MB
  uploadDataDuringCreation: true,
  removeFingerprintOnSuccess: true,
}).start();
```

## Routes

Paths are relative to `/storage/v1`. Bucket and key segments are percent-decoded. "Key" means
an anon key, a service key or a user token (see [Auth](#auth)), and "service" means the service
key. Signed URLs need nothing but their token.

### Buckets

| route | auth | body / query | answer |
|---|---|---|---|
| `GET /bucket` | key | `?search=&sortColumn=name\|created_at\|updated_at&sortOrder=asc\|desc&limit=&offset=` | `[Bucket]` |
| `POST /bucket` | service | `{id \| name, public?, file_size_limit?, allowed_mime_types?, type?, versioning_status?}` | `{name}` |
| `GET /bucket/{id}` | key | | `Bucket` |
| `PUT /bucket/{id}` | service | `{public?, file_size_limit?, allowed_mime_types?, versioning_status?}` | `{message}` |
| `DELETE /bucket/{id}` | service | | `{message}`; 409 `ResourceNotEmpty` if it has objects or branches |
| `POST /bucket/{id}/empty` | service | | `{message}` |

```text
Bucket = { id, name /* both the bucket's name */, owner: "", public, type: "STANDARD",
           file_size_limit: number | null, allowed_mime_types: [string] | null,
           versioning_status: "ENABLED" | "SUSPENDED" | "DISABLED", created_at, updated_at }
```

- Other callers see a bucket only if it's public or they may list its root (`s3:ListBucket`
  with an empty prefix). Otherwise it's `NoSuchBucket`. The service key sees all of its
  account's buckets.
- `search` is a substring match on the name.
- `type` other than `STANDARD` (analytics, vector) is 409 `FeatureNotEnabled`.
- `versioning_status`: `ENABLED`, `SUSPENDED` or `DISABLED`. Once versioning is enabled it can
  only be suspended, as on S3.
- `file_size_limit` and `allowed_mime_types`: see [Bucket limits](#bucket-limits). `null` removes
  them.
- `empty` moves every object to the trash. See [Differences](#differences-from-supabase-storage).

### Objects

| route | auth | answer |
|---|---|---|
| `POST /object/{bucket}/{key}` | key | upload; `{Id, Key: "<bucket>/<key>"}`; 409 `KeyAlreadyExists` without `x-upsert: true` |
| `PUT /object/{bucket}/{key}` | key | upload that replaces (upsert) |
| `GET\|HEAD /object/{bucket}/{key}` | key or none | the bytes (anonymous without a key) |
| `GET\|HEAD /object/public/{bucket}/{key}` | none | the bytes, if anyone may read them |
| `GET\|HEAD /object/authenticated/{bucket}/{key}` | key | the bytes |
| `GET /object/info/{bucket}/{key}` | key or none | `ObjectInfo` |
| `GET /object/info/public/{bucket}/{key}` | none | `ObjectInfo` |
| `GET /object/info/authenticated/{bucket}/{key}` | key | `ObjectInfo` |
| `DELETE /object/{bucket}/{key}` | key | `{message}` (`?versionId=` deletes one version) |
| `DELETE /object/{bucket}` | key | `{prefixes: [path \| {path, versionId}]}` (1–1000) → `[Object]` actually deleted |
| `POST /object/list/{bucket}` | key | `[Object \| Folder]` |
| `POST /object/list-v2/{bucket}` | key | `{hasNext, folders, objects, nextCursor?}` |
| `POST /object/move` | key | `{message, Id, Key}` |
| `POST /object/copy` | key | `Object & {Id, Key}` |

**Uploads.**
- The body is either the raw bytes or `multipart/form-data`, which is what storage-js sends for
  a `Blob`/`File`. A form's fields before the file count (`cacheControl` in seconds,
  `contentType`, `metadata` or `userMetadata` as JSON, `contentEncoding`). The first file is the
  object, and anything after it is ignored.
- Raw uploads take `Content-Type`, `Cache-Control` (as given, default `no-cache`),
  `Content-Encoding` and `x-metadata` (base64 of a JSON object: the user metadata).
- `x-upsert: true` replaces an existing object. Without it, an existing key is 409
  `KeyAlreadyExists`, checked again inside the commit (`If-None-Match: *`).
- A body without `Content-Length` is spooled first.
- A key must be 1–1024 bytes with no NUL (`InvalidKey` otherwise).
- The uploader's `sub` becomes the object's owner. See [Owner](#owner).

**Downloads.**
- `Range` gives 206 with `Content-Range`, and an unsatisfiable range is a real 416.
- `If-None-Match` gives a real 304.
- `?download` sets `Content-Disposition: attachment;`, and `?download=<name>` adds the file name
  (with an RFC 5987 `filename*`).
- `?versionId=` reads one version.
- The response sends `ETag`, `Last-Modified`, `Cache-Control`, `Content-Type`,
  `Content-Encoding`, `Accept-Ranges: bytes` and `X-Robots-Tag: none`. HEAD sends the headers
  only.

**Objects as JSON.**

```text
Object = { name /* full key, or relative to a v1 listing's prefix */, bucket_id, owner, owner_id /* both the uploader's sub, or null */,
           id /* UUID-shaped, per version */, version /* the version id; the object id in unversioned buckets */,
           created_at, updated_at, last_accessed_at /* all the last write */,
           metadata: { eTag, size, mimetype, cacheControl, lastModified, contentLength, httpStatusCode: 200, contentEncoding? },
           user_metadata: object | null }
ObjectInfo = { id, name, version, bucket_id, size, content_type, cache_control, content_encoding, etag,
               metadata /* the user metadata */, last_modified, created_at, owner, owner_id,
               is_versioned, is_delete_marker: false }
Folder = { name, id: null, updated_at: null, created_at: null, last_accessed_at: null, metadata: null }
```

**Lists.**
- `POST /object/list/{bucket}` takes `{prefix, limit (default 100, max 1500), offset, sortBy:
  {column, order}, search}`. It returns the folders and files directly under `prefix`, with
  names relative to it. Name order ascending reads only what the page needs. Any other order
  (descending, or by `created_at`, `updated_at` or `last_accessed_at`, which all sort by the last
  write) puts folders first and sorts at most 10,000 entries. `offset` is capped at 10,000.
- `POST /object/list-v2/{bucket}` takes `{prefix, cursor, with_delimiter (default false), limit
  (max 1000)}`. It uses cursor pagination in key order and returns full keys:
  `folders: [{key, name}]` and `objects: [Object & {key}]`.
- What a listing shows depends on the caller's rules. See [Listing](#listing).

**Move and copy.**
- Both take `{bucketId, sourceKey, destinationKey, destinationBucket?, sourceVersionId?}`.
- An existing destination is 409.
- A move within a bucket is a rename (one transaction, no bytes move). Across buckets, or from a
  version, it copies and then deletes the source. The object keeps its owner.
- `copy` also takes `copyMetadata` (default `true`) and `metadata: {cacheControl,
  contentEncoding, mimetype}`, plus the `x-upsert` and `x-metadata` headers. The copy shares the
  source's blob, and its owner is the caller's `sub` (the source's owner when the caller has
  none).

### Signed URLs

| route | auth | body / query | answer |
|---|---|---|---|
| `POST /object/sign/{bucket}/{key}` | key | `{expiresIn, transform?, versionId?}` | `{signedURL: "/object/sign/<bucket>/<key>?token=…"}` |
| `POST /object/sign/{bucket}` | key | `{expiresIn, paths: [1–1000]}` | `[{path, signedURL, error}]` |
| `GET\|HEAD /object/sign/{bucket}/{key}` | `?token=` | `&download=` | the bytes (with `Expires`) |
| `POST /object/upload/sign/{bucket}/{key}` | key | `x-upsert` | `{url: "/object/upload/sign/…?token=…", token}` |
| `PUT /object/upload/sign/{bucket}/{key}` | `?token=` | raw or form body | `{Id, Key}` |

- `expiresIn` is a whole number of seconds from 1 to 10 years. Signed upload URLs last 2 h.
- Signing checks that the caller may read the object (`s3:GetObject`) and that it exists. For an
  upload URL, it checks that the caller may write the key (`s3:PutObject`) and, without
  `x-upsert`, that the key is free.
- In a batch, paths the caller can't read get `error: "Either the object does not exist or you do
  not have access to it"`.
- The token is an HS256 JWT in Supabase's format: `{url: "<bucket>/<key>", scope: "download" |
  "upload", iat, exp}`, plus `versionId` for a version and `owner` and `upsert` for uploads. It's
  signed with a key derived from the deployment's key and bound to its path and scope. A
  download token without a scope (Supabase's older format) is accepted for downloads only.
- An upload through a signed URL belongs to the signer (`owner` in the token).
- `transform` in the body makes the URL a `/render/image/sign/...` URL (501 for now).

### Resumable uploads (TUS 1.0)

| route | what |
|---|---|
| `OPTIONS /upload/resumable` | `Tus-Version: 1.0.0`, `Tus-Extension: creation,creation-with-upload,creation-defer-length,termination,expiration`, `Tus-Max-Size` |
| `POST /upload/resumable` | create; 201 with `Location: <public url>/upload/resumable/<id>` and `Upload-Expires` |
| `HEAD /upload/resumable/{id}` | `Upload-Offset`, `Upload-Length` or `Upload-Defer-Length: 1`, `Upload-Metadata`, `Upload-Expires` |
| `PATCH /upload/resumable/{id}` | append at `Upload-Offset`; 204 with the new offset |
| `DELETE /upload/resumable/{id}` | terminate (abort) |
| `…/upload/resumable/sign[/{id}]` | the same routes, authorized by `x-signature: <signed upload token>` instead of a key |

- Every request needs `Tus-Resumable: 1.0.0`. Without it the answer is 412.
- `Upload-Metadata` carries `bucketName` and `objectName` (both required), plus `contentType`,
  `cacheControl` (seconds), `metadata` (JSON user metadata) and `contentEncoding`.
- The length comes from `Upload-Length`, or from `Upload-Defer-Length: 1` with `Upload-Length`
  declared later on a PATCH.
- `x-upsert: true` on create allows replacing an existing object. Without it, an existing object
  is 409 at create and again at completion.
- `PATCH` needs `Content-Type: application/offset+octet-stream` (415 otherwise) and the current
  `Upload-Offset` (409 otherwise). A body past `Upload-Length` is 400, past the bucket's size
  limit 413, and an expired upload 410.
- A `POST` with a body (`creation-with-upload`) takes its first bytes. `X-HTTP-Method-Override`
  on a `POST` is honoured.
- Creating needs `s3:PutObject` on the key. `HEAD` needs a key of the upload's project, and
  `PATCH` and `DELETE` need `s3:PutObject` too. A signed upload's owner and upsert come from its
  token.
- Errors use the real HTTP status (the protocol needs it).
- See [Resumable uploads](#resumable-uploads-design) for how they're stored.

### Image transformations

`GET /render/image/{public|authenticated|sign}/{bucket}/{key}?width=&height=&resize=cover|contain|fill&quality=&format=origin|avif|webp`

The routes are reserved, and their parameters and auth are checked (width and height 1–2500,
quality 20–100, a signed render URL's token). The transform itself answers a real
`501 NotImplemented` until the image engine is wired in.

### roll extras (`/roll/...`)

| route | auth | body | answer |
|---|---|---|---|
| `POST /roll/versions/{bucket}` | `s3:ListBucketVersions` | `{prefix, limit (≤1000, default 100), cursor}` | `{versions: [{name, version_id, is_latest, is_delete_marker, size, etag, last_modified}], next_cursor}` |
| `POST /roll/trash/{bucket}` | `s3:ListBucket` | `{prefix, limit, cursor}` | `{items: [{name, size, etag, deleted_at, expires_at, version_id}], next_cursor}` |
| `POST /roll/trash/{bucket}/restore` | `s3:PutObject` on the path | `{path, deletedAt}` | `ObjectInfo` |
| `GET /roll/branches/{bucket}` | may list the bucket | | `[Bucket]` |
| `POST /roll/branches/{bucket}` | service | `{name}` | `Bucket` |
| `GET /roll/policy/{bucket}` | service | | `{policy: document \| null}` |
| `PUT /roll/policy/{bucket}` | service | the policy document (≤ 20 KB) | `{message}` |
| `DELETE /roll/policy/{bucket}` | service | | `{message}` |
| `POST /roll/rules/{bucket}` | service | `{preset}` | `{policy}` (the policy it set) |

- `deleted_at` and `expires_at` are Unix ms.
- Policies are checked as `PutBucketPolicy` checks them. Bad ones are `MalformedPolicy`.

### Utility

| route | answer |
|---|---|
| `GET /version` | `roll-storage-api/<version>` (text) |
| `GET\|HEAD /status`, `/health` | 200, empty |
| anything else | `NotFound` (`Route <METHOD> /<path> not found`) |

## Errors

Errors have Supabase's shape:

```json
{ "statusCode": "404", "code": "NoSuchKey", "error": "not_found", "message": "Object not found" }
```

- **Client errors are HTTP 400.** The body's `statusCode` carries the real status, which is
  where storage-js reads it from (`error.statusCode`).
- **Server errors** use their real status: 500 `InternalError`, 503 `ServiceUnavailable`.
- **TUS answers** use the real status. So do 304, 416 and the renderer's 501.
- HEAD responses have no body.

| `statusCode` | `code` | when |
|---|---|---|
| 400 | `InvalidRequest`, `InvalidKey`, `InvalidBucketName`, `MissingParameter`, `MalformedPolicy` | bad input |
| 400 | `InvalidJWT` | a key or token that doesn't verify, expired, rotated away |
| 400 | `InvalidSignature`, `ExpiredToken` | signed URLs |
| 403 | `AccessDenied` | no credentials; a write the rules refuse (`new row violates row-level security policy`); a service-key route |
| 404 | `NoSuchBucket`, `NoSuchKey`, `NoSuchUpload`, `NotFound` | missing, or a read the rules refuse |
| 405 | `MethodNotAllowed` | |
| 409 | `KeyAlreadyExists`, `BucketAlreadyExists`, `ResourceNotEmpty`, `FeatureNotEnabled` | |
| 413 | `EntityTooLarge` | over the bucket's `file_size_limit` or the API's maximum |
| 415 | `InvalidMimeType` | not in `allowed_mime_types` |
| 423 | `ResourceLocked` | Object Lock |

## Auth

Requests carry `apikey: <key>` and `Authorization: Bearer <token>`, as Supabase's gateway takes
them. `?apikey=` works too. Tokens are never logged or echoed in errors.

### Project keys

- Each account has an **anon key** and a **service key**. Both are HS256 JWTs: `{iss: "roll",
  ref: <account id>, role: "anon" | "service_role", gen, iat, exp}`, valid for 10 years.
- They're signed with a key derived (HMAC-SHA256) from the deployment's API key. That key is 32
  random bytes made on first use and kept in `state` (`storage_api_key`).
- The keys themselves are kept AES-256-GCM-sealed in `api_projects` (the session tokens'
  scheme) under another derived key, so the dashboard can show them again.
- **Rotation** (`POST /v1/storage/api/keys/rotate`) bumps the project's generation. The keys
  carry it, so old keys stop working at once.
- A project key in `Authorization`, or alone in `apikey`, makes the request the project's anon
  role or service role.
- **The service key acts as the account's root user.** Like Supabase's `service_role`, it gets
  past the rules: everything in its account is allowed. An explicit `Deny` in a bucket policy
  still applies. Only the service key can create, change, empty or delete buckets, branch them,
  or set their policies.
- **The anon key** is the `anon` role, with no `sub`. It's public, so treat what it can do as
  public.

### User tokens

Any other bearer token is a user token of the project named by `apikey`. That `apikey` (the anon
or service key) is required, as on Supabase. A user token is verified by one of the project's
**auth providers**:

- **HS256 with the project's JWT secret**, like Supabase's JWT secret. It must be 32–512
  characters. You can also require an `iss` and an `aud` (`jwt_issuer`, `jwt_audience`).
- **RS256 or ES256 from an OIDC issuer.** Up to 10 issuers, each with its accepted audiences, an
  optional `sub` pattern and an optional `jwks_uri`; without one, OpenID discovery finds the
  keys. Keys are fetched only over HTTPS, with timeouts and size caps, and cached (the same
  verifier as `AssumeRoleWithWebIdentity`).
- `exp` and `nbf` are checked with 30 s of leeway.

The token's `role` claim is the role that bucket policies name. Without one it's
`authenticated`. A `role: "service_role"` in a user token is just a role name: it never gets the
service key's powers. The token's claims become `jwt:*` condition keys, and its `sub` owns what
the user uploads.

The JWT secret is write-only. It's sealed with the keys and never returned. Set the providers
with `PUT /v1/storage/api/auth`, or on the dashboard.

## Rules (bucket policies)

What anyone but the service key may do is decided by the bucket's policy, through the S3 API's
own evaluator (`access.rs`, the same one behind S3 requests, the policy simulator and the
dashboard). Callers with the anon key or a user token are identities with no identity policies
of their own, so in their account only resource grants let them in: the bucket policy, or the
`public` flag for reads.

- **Principals:**
  - `{"App": "<role>"}`: `anon` (the anon key), `authenticated`, any other JWT `role`, or `*`
    for any of them. Role names are letters, digits, `_`, `-` and `*`, up to 64 characters.
  - `"*"` or `{"AWS": "*"}`: anyone, including requests with no key at all (public URLs).
    `{"App": "anon"}` only matches requests that carry the anon key.
- **Condition keys and policy variables:**
  - `jwt:<claim>`: any claim, matched case-insensitively, with a dotted path into nested
    objects (`jwt:app_metadata.plan`). An array is many values, and objects and `null` are
    absent. Use them as policy variables too: `${jwt:sub}`, `${jwt:email}`.
  - `roll:ObjectOwner`: the owner of the object now at the key, which is the `sub` that wrote
    it. It's `""` for objects written otherwise and absent when there's no object.
  - The usual keys apply as well: `aws:SourceIp`, `aws:Referer`, `aws:UserAgent`,
    `aws:CurrentTime`, `s3:prefix`, …. `aws:PrincipalType` is `WebIdentityUser`, `aws:userid` is
    the `sub` (or the role), and `aws:PrincipalArn` is
    `arn:roll:storage-api::<account>:role/<role>`.
- **Actions** are S3's:

  | route | action |
  |---|---|
  | downloads, info, signing download URLs | `s3:GetObject` |
  | uploads, signing upload URLs, TUS, restore from the trash | `s3:PutObject` |
  | deletes | `s3:DeleteObject` |
  | move | `s3:GetObject` and `s3:DeleteObject` on the source, `s3:PutObject` on the destination |
  | copy | `s3:GetObject` on the source, `s3:PutObject` on the destination |
  | lists, `GET /bucket`, branches | `s3:ListBucket` (with `s3:prefix`) |
  | versions | `s3:ListBucketVersions` |

- **Refusals:**
  - A refused write is 403 `new row violates row-level security policy`, as Supabase answers
    it.
  - A refused read or delete is `NoSuchKey`, because the caller doesn't learn about objects it
    can't see. Without a key it's `NoSuchBucket`.
  - A batch delete leaves out what the caller may not delete.
- **Other accounts' buckets are never reachable** with a key or token. The project's own side
  allows nothing, and across accounts both sides must. Only anonymous requests and signed URLs
  address buckets by name alone.
- **Block Public Access** counts `"*"`, `{"AWS": "*"}`, `{"App": "anon"}` and `{"App": "*"}` as
  public, since the anon key is public. `BlockPublicPolicy` refuses such a policy.

### Listing

- A caller with `s3:ListBucket` on the listing's prefix (the `s3:prefix` it asks for) sees
  everything under it.
- Anyone else sees only the files it may read (`s3:GetObject`) and the folders it may list
  (`s3:ListBucket` with the folder as prefix). So a user-folders bucket listed at its root shows
  each user only their own folder.
- One listing request reads at most 100,000 keys (a sparse view of a big bucket can come back
  short).

### Presets

`POST /roll/rules/{bucket}` (`applyRules`), `POST /v1/storage/buckets/{name}/rules` and the
dashboard set these as the bucket's policy. They replace any policy it had. Shown here for a
bucket named `avatars`; key order in the stored document may differ.

**`public-read`.** Anyone can download, with or without a key, and public URLs work. Uploads
need the service key. This is a public policy, so Block Public Access refuses it.

```json
{
  "Version": "2012-10-17",
  "Statement": [
    { "Sid": "PublicRead", "Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::avatars/*" }
  ]
}
```

**`authenticated-read`.** Signed-in users (any valid user token whose role is `authenticated`)
list and download. Uploads need the service key.

```json
{
  "Version": "2012-10-17",
  "Statement": [
    { "Sid": "AuthenticatedRead", "Effect": "Allow", "Principal": { "App": "authenticated" }, "Action": "s3:GetObject", "Resource": "arn:aws:s3:::avatars/*" },
    { "Sid": "AuthenticatedList", "Effect": "Allow", "Principal": { "App": "authenticated" }, "Action": "s3:ListBucket", "Resource": "arn:aws:s3:::avatars" }
  ]
}
```

**`user-folders`.** Signed-in users list, read, write and delete only under `avatars/<their
sub>/`.

```json
{
  "Version": "2012-10-17",
  "Statement": [
    { "Sid": "OwnFolder", "Effect": "Allow", "Principal": { "App": "authenticated" }, "Action": ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"], "Resource": "arn:aws:s3:::avatars/${jwt:sub}/*" },
    { "Sid": "ListOwnFolder", "Effect": "Allow", "Principal": { "App": "authenticated" }, "Action": "s3:ListBucket", "Resource": "arn:aws:s3:::avatars", "Condition": { "StringLike": { "s3:prefix": ["${jwt:sub}/*"] } } }
  ]
}
```

**`owner-only`.** Signed-in users upload anywhere, and read, replace and delete only the objects
they uploaded.

- `StringEqualsIfExists` lets a user write a key that has no object yet.
- An object written by the service key or through the S3 API has owner `""`, so no user can
  touch it.
- There's no `s3:ListBucket` grant, so lists show a user only their own files. Folders are hidden
  unless their files are listed with `list-v2` without a delimiter.

```json
{
  "Version": "2012-10-17",
  "Statement": [
    { "Sid": "OwnerReadDelete", "Effect": "Allow", "Principal": { "App": "authenticated" }, "Action": ["s3:GetObject", "s3:DeleteObject"], "Resource": "arn:aws:s3:::avatars/*", "Condition": { "StringEquals": { "roll:ObjectOwner": "${jwt:sub}" } } },
    { "Sid": "OwnerWrite", "Effect": "Allow", "Principal": { "App": "authenticated" }, "Action": "s3:PutObject", "Resource": "arn:aws:s3:::avatars/*", "Condition": { "StringEqualsIfExists": { "roll:ObjectOwner": "${jwt:sub}" } } }
  ]
}
```

Write your own policy for anything else. For example, paying users may read `premium/`:

```json
{ "Effect": "Allow", "Principal": { "App": "authenticated" }, "Action": "s3:GetObject",
  "Resource": "arn:aws:s3:::media/premium/*",
  "Condition": { "StringEquals": { "jwt:app_metadata.plan": "pro" } } }
```

## Owner

An object's owner is Supabase's `owner_id`: the `sub` of the user who uploaded it. It's shown as
both `owner` and `owner_id`.

- **Where it's kept.** It's a column of the object's row (`objects`, `versions`, `trash`,
  `uploads`), so every copy of the row carries it: branch push-down, noncurrent versions, the
  trash and restores, and multipart and TUS uploads from create to complete.
- **Who sets it:**
  - Uploads, signed upload URLs (the signer) and resumable uploads set it to the user's `sub`.
  - The anon key, the service key, the S3 API and the control plane write no owner.
  - A copy belongs to the caller who copies it, and a move keeps the original owner.
  - A copy through the S3 API (`CopyObject`) keeps the source's owner.
- **How rules use it.** `roll:ObjectOwner` reads it (`""` when there's none).

## Bucket limits

These are Supabase's bucket settings. They're stored as a `bucket_config` row (kind `limits`)
and copied to branches like the rest of a bucket's configuration.

- **`file_size_limit`:** bytes as a number, or a string with decimal units: `"20MB"`, `"1.5GB"`,
  `"30KB"`, `"3B"`. `null` means no limit beyond the API's own 5 GiB per request and 5 TiB
  resumable.
- **`allowed_mime_types`:** up to 100 media types, exact (`image/png`) or by type (`image/*`).
  They're normalized to lowercase `type/subtype` with parameters dropped, and duplicates are
  removed. `null` or `[]` allows any type. A write without a `Content-Type` counts as
  `application/octet-stream`.
- `.emptyFolderPlaceholder` objects (Supabase's folders) are exempt.

**Enforcement.** The store checks the limits inside the transaction that writes any new version
(`versioning::put_key`), so every write path honours them: this API, its resumable uploads, the
S3 API (PUT, POST, copies, multipart completes, appends, renames) and the control plane. Checks
before any bytes move fail sooner:

- here, before reading the body: the type, and `Content-Length` against the size limit
- a resumable create checks `Upload-Length` and the type, and a deferred length is checked when
  it's declared
- the S3 API's PUT checks both, `CreateMultipartUpload` checks the type, and `UploadPart` refuses
  a part that alone exceeds the size limit

On this API, a refused write is `413 EntityTooLarge` or `415 InvalidMimeType`, in the 400 body.
On the S3 API it's `EntityTooLarge` (400) or `InvalidRequest` (`mime type <type> is not
supported`); see [compatibility.md](compatibility.md#developer-api-limits-and-rules-seen-from-s3).

Set limits with `POST`/`PUT /bucket`, the control plane's bucket PATCH, or the dashboard's bucket
settings (Upload limits).

## Resumable uploads (design)

A resumable upload is a multipart upload of the store, so it completes into one blob like every
object, plus an `api_uploads` row with the same id.

- **What's stored.** The row holds the account, the declared length (or none, deferred), the part
  size, whether it may overwrite, whether it was signed, the `Upload-Metadata` header, and when it
  expires.
- **Lifecycle.** The row is written with the multipart upload and removed in the transaction that
  completes or aborts it, so the two never disagree. The headers (type, cache control, user
  metadata, owner) are the upload's, as for any multipart upload.
- **Part size.** The store's minimum (5 MiB), or `length / 10,000` rounded up for objects over
  about 48 GiB.
- **Buffering.** Bytes arrive in any sizes and are appended to a buffer file on local disk,
  `<spool>/tus/<id>.<part number>`. When the buffer holds a full part, it's fsynced, uploaded as
  that part (a backend `UploadPart` plus a durable `parts` row), and deleted. Each upload in
  progress buffers at most one part on disk.
- **Offset.** It's the recorded parts' bytes plus the current buffer file's length.
  - After a restart on the same disk, an upload resumes exactly where it was.
  - On another node it resumes from its last recorded part. The client's `HEAD` sees the smaller
    offset and sends the rest again, as TUS clients do.
  - A buffer file is named by its part number, so one whose part was recorded just before a crash
    is ignored.
- **Completion.** When the last byte arrives, the remaining buffer becomes the last part (which
  may be under 5 MiB). The upload completes with `If-None-Match: *` unless it may overwrite, and
  the bucket's limits are checked again in that commit. An empty upload is written as an empty
  PUT.
- **Concurrency.** Requests to one upload run one at a time on a node.
- **Expiry.** 24 h after creation (`Upload-Expires`). A `PATCH` after it is 410. A sweeper (every
  10 minutes) aborts expired uploads and removes their buffers. Aborting goes through the store's
  normal abort, so the backend upload and its parts go too.

## Metering

This API meters like the S3 API (`s3-server`'s `Meter`), per bucket:

- **class A:** uploads (each upload, and each TUS request that carries bytes), lists, copies,
  moves, bucket creates and updates, branch creation, policy writes, trash and version lists,
  and restores
- **class B:** downloads, `info`, and signing download or upload URLs
- **free:** deletes
- **bytes in:** uploaded bytes
- **egress:** downloaded bytes, counted as **internal** when the peer is inside roll
  (`ROLL_S3_INTERNAL_CIDRS`) and as **internet** otherwise

Prices are the S3 API's ([pricing](README.md#pricing)).

## Control plane and dashboard

The control plane is the roll API (`/v1/storage`, see
[architecture.md](architecture.md#control-plane-api-s3-control-v1storage)). Its endpoints act in
one account, as everywhere else there.

```text
GET    /v1/storage/api
  → { account_id, endpoint, anon_key, service_key, key_generation, keys_issued_at,
      auth: { jwt_secret_set, jwt_issuer, jwt_audience, oidc: [OidcProvider] },
      presets: [{ id, name, description }] }
POST   /v1/storage/api/keys/rotate                         → the same view, new keys
PUT    /v1/storage/api/auth  { jwt_secret?, jwt_issuer?, jwt_audience?, oidc? }   → the view
       omitted: unchanged; null: removed; oidc replaces the list
       OidcProvider = { issuer /* the tokens' iss, exactly */, audiences: [string] (1–20), subject? /* * ? pattern */, jwks_uri? }
GET    /v1/storage/buckets/{name}/rules/{preset}           → { preset, policy }   (a preview)
POST   /v1/storage/buckets/{name}/rules  { preset }        → Bucket                (sets the policy)
PATCH  /v1/storage/buckets/{name}  { …, file_size_limit?: number | string | null,
                                     allowed_mime_types?: [string] | null }      → Bucket
```

- The project and its keys are created on first use.
- `/v1/storage/api` and its key and auth routes are the operator's. A signed-in IAM user gets
  403 `OperatorOnly`. The rules routes are authorized as `s3:GetBucketPolicy` and
  `s3:PutBucketPolicy` on the bucket.
- `Bucket` includes `file_size_limit` and `allowed_mime_types`.
- The dashboard's **Storage → API** page (`/dashboard/storage/-/api`) shows the endpoint, a
  quickstart for `@roll/storage` and storage-js, and the keys (masked, copyable, rotatable). It
  also sets the JWT secret and OIDC issuers and applies rule presets to buckets. Each bucket's
  settings have an **Upload limits** panel.

## Migrating from Supabase Storage

1. **Get the endpoint and keys.** Use the dashboard's API page or `GET /v1/storage/api`.

2. **Make your existing user tokens verify.**
   - If your project signs with the **legacy JWT secret** (HS256), set it:

     ```sh
     curl -X PUT http://127.0.0.1:7070/v1/storage/api/auth -H 'content-type: application/json' -d '{
       "jwt_secret": "<Supabase JWT secret>",
       "jwt_issuer": "https://<ref>.supabase.co/auth/v1",
       "jwt_audience": "authenticated" }'
     ```

   - If it uses **asymmetric signing keys** (RS256/ES256), add Supabase Auth as an OIDC issuer:

     ```json
     { "oidc": [{ "issuer": "https://<ref>.supabase.co/auth/v1", "audiences": ["authenticated"],
                  "jwks_uri": "https://<ref>.supabase.co/auth/v1/.well-known/jwks.json" }] }
     ```

   - Any other OIDC provider (Auth0, Clerk, Google, …) works the same way. Their tokens usually
     have no `role` claim and so act as `authenticated`.

3. **Create the buckets** with the service key, with the same public flag and limits. Names
   must follow S3's rules (3–63 characters: lowercase letters, digits, `-`, `.`) and be free in
   the deployment, so rename any that don't fit.

   ```ts
   await storage.createBucket("avatars", { public: false, fileSizeLimit: "5MB", allowedMimeTypes: ["image/*"] });
   ```

4. **Recreate your RLS policies as bucket policies.** Start from a preset, or write a policy:

   | Supabase (policies on `storage.objects`) | roll |
   |---|---|
   | public bucket | `public: true`, or the `public-read` preset |
   | `for select to authenticated using (bucket_id = 'b')` | `authenticated-read` |
   | `… using ((storage.foldername(name))[1] = auth.uid()::text)` for select, insert, update, delete | `user-folders` |
   | `… using (owner_id = auth.uid()::text)` (or `owner = auth.uid()`) | `owner-only` |
   | `for insert to authenticated with check (bucket_id = 'b')` | `{"Principal": {"App": "authenticated"}, "Action": "s3:PutObject", "Resource": "arn:aws:s3:::b/*"}` |
   | `… to anon` | `{"App": "anon"}` (only with the anon key; `"*"` for keyless public URLs) |
   | `… to <custom role>` | `{"App": "<role>"}` (the JWT's `role` claim) |
   | `auth.jwt() ->> 'email' = '…'` | `"Condition": {"StringEquals": {"jwt:email": "…"}}` |
   | `auth.jwt() -> 'app_metadata' ->> 'plan' = 'pro'` | `"Condition": {"StringEquals": {"jwt:app_metadata.plan": "pro"}}` |
   | `storage.extension(name) = 'png'` | `"Resource": "arn:aws:s3:::b/*.png"` |
   | `name like 'public/%'` | `"Resource": "arn:aws:s3:::b/public/*"` |
   | select + insert + update for upserts | `s3:PutObject` alone (replacing needs no read) |
   | `service_role` bypasses RLS | the service key (an explicit `Deny` still applies) |

   Policies that join other tables have no equivalent. Put what they decide into the token's
   claims (`app_metadata`) and use a `jwt:` condition.

5. **Copy the data.** Pick one of these:
   - **rclone through the two S3 APIs.** This is the fastest way. Supabase's S3 endpoint is
     `https://<ref>.supabase.co/storage/v1/s3`, with access keys from its storage settings. On
     the roll side, make a key with `roll storage keys create migration --permission write`.

     ```ini
     [supabase]
     type = s3
     provider = Other
     endpoint = https://<ref>.supabase.co/storage/v1/s3
     region = <project region>
     access_key_id = …
     secret_access_key = …
     force_path_style = true

     [roll]
     type = s3
     provider = Other
     endpoint = http://127.0.0.1:59000
     access_key_id = RK…
     secret_access_key = …
     force_path_style = true
     ```

     ```sh
     rclone sync supabase:avatars roll:avatars --transfers 32 --progress
     rclone check supabase:avatars roll:avatars --size-only
     ```

     Objects copied through S3 have **no owner**, and Supabase's `user_metadata` (kept in its
     database) doesn't come along. If your rules use the owner (`owner-only`) or your app reads
     `user_metadata`, use a script instead.
   - **A script with both clients.** It lists with Supabase's service role key, downloads, and
     uploads to roll:

     ```ts
     import { createClient as supabaseClient } from "@supabase/supabase-js";
     import { createClient as rollClient } from "@roll/storage";

     const from = supabaseClient(SUPABASE_URL, SUPABASE_SERVICE_ROLE_KEY).storage;
     const to = rollClient(ROLL_STORAGE_URL, ROLL_SERVICE_KEY);

     async function* walk(bucket: string, prefix = ""): AsyncGenerator<{ path: string; obj: any }> {
       for (let offset = 0; ; offset += 1000) {
         const { data, error } = await from.from(bucket).list(prefix, { limit: 1000, offset });
         if (error) throw error;
         for (const obj of data) {
           const path = prefix ? `${prefix}/${obj.name}` : obj.name;
           if (obj.id === null) yield* walk(bucket, path); // a folder
           else yield { path, obj };
         }
         if (data.length < 1000) return;
       }
     }

     for await (const { path, obj } of walk("avatars")) {
       const { data: blob, error } = await from.from("avatars").download(path);
       if (error) throw error;
       const { error: e } = await to.from("avatars").upload(path, blob, {
         contentType: obj.metadata?.mimetype,
         cacheControl: obj.metadata?.cacheControl?.match(/max-age=(\d+)/)?.[1],
         upsert: true,
       });
       if (e) throw e;
     }
     ```

   - **Keeping owners.** The service key writes objects without an owner. To keep each object's
     owner, upload it with a short-lived token for that owner instead: an HS256 JWT `{sub:
     <owner_id>, role: "authenticated", exp}` signed with the project's JWT secret (plus the
     `iss`/`aud` you required; if your users sign in through OIDC, set a temporary secret for the
     copy), sent with the anon key as `apikey`. The bucket's rules must let
     that user write the key, which `owner-only` and `user-folders` do. Pass `metadata` to
     `upload` to carry `user_metadata`.
   - Files over 5 GiB need a resumable upload.

6. **Point the app at roll.** Change the storage URL and key: `@roll/storage`'s
   `createClient(rollUrl, rollAnonKey, { accessToken })`, or storage-js with roll's URL and
   anon key. Supabase Auth keeps issuing the tokens.

7. **Cut over.** Run the copy again just before switching, to pick up new files.

## Differences from Supabase Storage

- **Bucket ids follow S3's naming rules** (3–63 characters, lowercase letters, digits, `-` and
  `.`) and are unique across the deployment, not per project. A bucket's `id` and `name` are the
  same.
- **`user_metadata`** is kept as one S3 user metadata entry, `user-metadata` (the JSON text, with
  non-ASCII escaped). It's limited to S3's 2 KB of user metadata in total, where Supabase allows
  1 MB. Objects written through the S3 API show their `x-amz-meta-*` as `user_metadata` (a map of
  strings).
- **`search` in `list`** is a case-sensitive prefix match on the names under `prefix`. Supabase's
  is case-insensitive.
- **Sorting a list by anything but name ascending** (`created_at`, `updated_at`,
  `last_accessed_at`, or descending) sorts at most 10,000 entries. `offset` is capped at 10,000.
  One listing request reads at most 100,000 keys.
- **Object `id`** is a UUID-shaped hash of the bucket, key and version, so every write makes a new
  one. `created_at`, `updated_at` and `last_accessed_at` are all the time of the last write.
- **Move keeps the object's owner.** Supabase sets it to the caller who moves it. Copy behaves as
  on Supabase: the copy belongs to the caller.
- **A bucket's `owner` is `""`.** Buckets belong to the account. Its `updated_at` is its
  `created_at`.
- **`emptyBucket` moves everything to the trash,** where it stays restorable for the bucket's
  `recover_days`. It answers once the bucket is empty or after 10 s, whichever comes first, and
  keeps going in the background. Deletes in general go to the trash too, and trashed bytes are
  billed as storage.
- **Image transformations answer 501** until the image engine is wired in. That covers
  `/render/image/...`, and `createSignedUrl` with `transform` makes such URLs.
- **There are no `/cdn` purge routes,** and the API doesn't serve S3 under `/storage/v1/s3`.
  roll's S3 API is its own listener (`:59000`).
- **Analytics (Iceberg) and vector buckets aren't supported.** A `type` other than `STANDARD` is
  409 `FeatureNotEnabled`.
- **RLS is replaced by bucket policies** (see [Rules](#rules-bucket-policies)). Refused reads are
  `NoSuchKey`, and refused writes keep Supabase's message.
- **Grants to the `anon` role count as public** for Block Public Access, because the anon key is
  public.
- **`service_role` in a user token is only a role name.** The service key alone bypasses the
  rules.
- **Signed URL tokens** are signed with roll's own key, not your JWT secret, and are bound to
  their path and scope.
- **Resumable uploads take chunks of any size.** Supabase requires 6 MB.
- **Versions** extend the API: `versioning_status` on buckets, `?versionId=` on reads, info and
  deletes, `versionId` in signing and batch deletes, `sourceVersionId` in move and copy.
