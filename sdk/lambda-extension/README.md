# roll secrets' AWS Lambda extension

A Lambda layer that runs beside a function and answers a roll secrets environment's values on
`127.0.0.1:2773`, from a cache, as AWS's Parameters and Secrets Lambda Extension does for Secrets
Manager. The function holds no roll token, and an invocation doesn't call roll: values are read
when the sandbox starts (`ROLL_PREFETCH`) and then kept for `ROLL_CACHE_TTL`. See
docs/secrets.md, "AWS Lambda".

The extension logs in to roll as a workload identity with a token AWS STS signs for the function's
own IAM role, so nothing long-lived sits in the function's configuration either. It uses only the
standard library and the Go client (`sdk/go/secrets`).

## Build

With Go 1.24:

```sh
make            # layer-amd64.zip and layer-arm64.zip, the binary at extensions/roll-secrets
make clean
```

Without Go, from this directory:

```sh
docker run --rm -v "$PWD/..":/sdk -w /sdk/lambda-extension golang:1.24-bookworm make
```

Each zip holds one static binary (`GOOS=linux`, `CGO_ENABLED=0`, about 2.6 MB zipped), so the layer
works with every runtime, including `provided.al2023` and container images that copy
`/opt/extensions` in.

## Publish and attach

```sh
aws lambda publish-layer-version --layer-name roll-secrets-arm64 \
  --zip-file fileb://layer-arm64.zip --compatible-architectures arm64
aws lambda update-function-configuration --function-name orders \
  --layers arn:aws:lambda:eu-west-1:111122223333:layer:roll-secrets-arm64:1 \
  --environment 'Variables={ROLL_API_URL=https://api.roll.example,ROLL_IDENTITY=idn_…,ROLL_ENV=production,ROLL_PREFETCH=production:/}'
```

Publish the zip that matches the function's architecture (`x86_64` takes `layer-amd64.zip`).
`update-function-configuration --environment` replaces all of the function's variables, so give
every variable the function needs.

## Logging in

### A workload identity through AWS STS (recommended)

The extension calls STS's `GetWebIdentityToken` (IAM's outbound identity federation) with the
function's own credentials. STS answers with a JWT for audience `ROLL_API_URL`, signed RS256 and
valid for a minute. The extension exchanges it for a roll login (`POST /v1/secrets/login`), which
lasts the identity's TTL, and logs in the same way again before that login ends.

1. Turn on outbound identity federation for the account, once, and note its issuer:

   ```sh
   aws iam enable-outbound-web-identity-federation
   aws iam get-outbound-web-identity-federation-info   # IssuerIdentifier: https://<id>.tokens.sts.global.api.aws
   ```

2. Let the function's execution role ask for a token for roll's audience:

   ```json
   {
     "Effect": "Allow",
     "Action": "sts:GetWebIdentityToken",
     "Resource": "*",
     "Condition": {
       "ForAnyValue:StringEquals": { "sts:IdentityTokenAudience": "https://api.roll.example" },
       "NumericLessThanEquals": { "sts:DurationSeconds": 300 }
     }
   }
   ```

3. Make a workload identity that trusts that role. The token's `sub` is the role's ARN, including
   its path:

   ```sh
   roll secrets identities add orders-fn --for-env production \
     --issuer https://<id>.tokens.sts.global.api.aws \
     --audience https://api.roll.example \
     --subject arn:aws:iam::111122223333:role/orders-fn
   ```

4. Set `ROLL_IDENTITY` to the identity's id (`idn_…`).

Each AWS account has its own issuer URL, so an identity can't be reached from another account.
Everything that runs as the role can log in as the identity, so give each function its own role.

### Other ways

- **`ROLL_TOKEN`**: a roll token (`roll secrets tokens create orders --for-env production --action
  read --action reveal`). Lambda encrypts environment variables at rest. However, the function's
  code and anyone allowed `lambda:GetFunctionConfiguration` can read the token, so prefer an
  identity.
- **`ROLL_IDENTITY` with `ROLL_OIDC_TOKEN` or `ROLL_OIDC_TOKEN_FILE`**: an OIDC token from
  somewhere else. The extension reads the file again for each login.

## Configuration

| Variable | Default | |
| --- | --- | --- |
| `ROLL_API_URL` | (required) | rolld's API, e.g. `https://api.roll.example` |
| `ROLL_IDENTITY` | | the workload identity to log in as (see above) |
| `ROLL_TOKEN` | | a roll token instead |
| `ROLL_OIDC_AUDIENCE` | `ROLL_API_URL` | the audience STS signs the token for (the identity's `audience`) |
| `ROLL_PROJECT` | the token's | the project (`prj_…`), when the token isn't limited to one |
| `ROLL_ENV` | | the environment when a request names none |
| `ROLL_PREFETCH` | | folders loaded before the first invocation: `production:/,staging:/api` |
| `ROLL_CACHE_TTL` | `300` | seconds (or a duration, `5m`) a folder is answered from memory; `0` turns the cache off |
| `ROLL_CACHE_SIZE` | `1000` | how many folders are kept; past it, the one read longest ago goes; `0`: no cap |
| `ROLL_EXTENSION_PORT` | `2773` | the port on 127.0.0.1 (AWS's extension uses 2773 too: change one if both are attached) |

## The API

Every request carries the function's session token, `X-Roll-Token: $AWS_SESSION_TOKEN`. AWS's
extension requires `X-Aws-Parameters-Secrets-Token` in the same way. Other code in the sandbox can
reach localhost, but without the function's credentials it reads nothing. A request without the
token, or with a different one, gets a 401.

- `GET /secrets?env=production&path=/` answers the folder's values as a JSON object, `{"KEY":
  "value", …}`.
- `GET /secrets/{KEY}?env=production&path=/` answers one value, as text.
- `GET /health` answers 200 once the extension is logged in to roll, and 503 with the reason
  otherwise.

`path` defaults to `/`, and `env` defaults to `ROLL_ENV`. An error answers
`{"error": …, "code": …}`, which names keys and folders but never a value:

- 404: the folder has no such key.
- 409 `unresolved`: a key doesn't resolve. `/secrets` answers a folder only when every key in it
  resolves.
- 403, 404 or 429: roll's own answers.
- 502: anything else from roll (roll is down, or the login was refused).

Every answer is `Cache-Control: no-store`.

### Node

```js
const roll = async (path) => {
  const res = await fetch(`http://127.0.0.1:2773${path}`, { headers: { "X-Roll-Token": process.env.AWS_SESSION_TOKEN } });
  if (!res.ok) throw new Error(`roll secrets: ${res.status} ${(await res.json()).error}`);
  return res;
};
export const handler = async () => {
  const env = await (await roll("/secrets?env=production")).json();
  const dbUrl = await (await roll("/secrets/DATABASE_URL?env=production")).text();
  // …
};
```

### Python

```python
import json, os, urllib.request

def roll(path):
    req = urllib.request.Request("http://127.0.0.1:2773" + path, headers={"X-Roll-Token": os.environ["AWS_SESSION_TOKEN"]})
    with urllib.request.urlopen(req, timeout=5) as res:
        return res.read()

def handler(event, context):
    env = json.loads(roll("/secrets?env=production"))
    db_url = roll("/secrets/DATABASE_URL?env=production").decode()
```

### Go

```go
func roll(ctx context.Context, path string) ([]byte, error) {
	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, "http://127.0.0.1:2773"+path, nil)
	req.Header.Set("X-Roll-Token", os.Getenv("AWS_SESSION_TOKEN"))
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, err
	}
	defer res.Body.Close()
	if res.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("roll secrets: %s", res.Status)
	}
	return io.ReadAll(res.Body)
}

// values, err := roll(ctx, "/secrets?env=production")  → json.Unmarshal into map[string]string
```

Use `127.0.0.1`, not `localhost`. The extension listens on IPv4 only, and some runtimes try `::1`
first.

## Caching

- **A folder without dynamic credentials** is kept for `ROLL_CACHE_TTL` (the Go client's
  `CacheTTL`). After that, the extension asks roll again with the folder's digest. While the
  folder hasn't changed, roll answers 304: nothing is decrypted, issued or audited, and the kept
  values stay for another TTL.
- **A folder holding a dynamic credential** (a Postgres login per lease, AWS keys) is never kept
  past its lease. The values answer doesn't say when a lease ends, so the extension renews each
  lease once, right after the read, to learn it. It then keeps the folder until a fifth of the
  first lease's life (at least a minute) is left, and never past the TTL. A lease that can't be
  renewed leaves the folder unkept. A lease that ends while the sandbox is frozen isn't renewed:
  the next read gets a new lease.
- **A folder with a key that doesn't resolve** is never kept.

Nothing is written to disk. A value never goes in a log or an error: the extension logs folders,
keys, lease ids and roll's error messages only. The server binds 127.0.0.1 only. SHUTDOWN closes
the server within Lambda's deadline.

## Gaps

- **Not run on AWS yet.** The STS call follows AWS's `GetWebIdentityToken` reference (query API on
  the region's endpoint, `Audience.member.1`, `SigningAlgorithm=RS256`, `DurationSeconds`) and is
  signed with Signature Version 4. The signer reproduces AWS's own example signature in the tests.
  However, nobody has logged a real function in through an account's issuer yet. roll's side should
  take the token as it is: the issuer is a public HTTPS URL with an OIDC discovery document, roll
  verifies RS256 (STS's ES384 isn't one of roll's algorithms, which is why the extension asks for
  RS256), and `aud` may be a string or a list.
- **Only the role can be pinned.** roll's identity `claims` match top-level string claims. AWS's own
  claims (`aws_account`, `lambda_source_function_arn`, principal tags) are nested under
  `https://sts.amazonaws.com/`, so an identity can't require a particular function. Give each
  function its own role.
- No sealed resolve (`resolve` with `seal_to`): the values are in the clear only between roll and
  the extension (over HTTPS) and on the loopback.
- No push: a change reaches a warm sandbox after at most `ROLL_CACHE_TTL`.

## Tests

The tests use the standard library, with a fake Extensions API, a fake rolld and a fake STS.
They cover register → prefetch → INVOKE → SHUTDOWN, the token check, the TTL and its 304,
dynamic credentials within their lease, and SigV4 against AWS's example. Run them with:

```sh
make test     # gofmt, go vet, go test -race
docker run --rm -v "$PWD/..":/sdk -w /sdk/lambda-extension golang:1.24-bookworm make test
```
