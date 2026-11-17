using System.Collections.Concurrent;
using System.Net;
using System.Net.Http.Headers;
using System.Security.Cryptography;
using System.Text.Json;

namespace Roll.Secrets;

/// <summary>
/// A client for roll secrets (docs/secrets.md, "Consumption"): an environment's values, the runtime
/// resolve (sealed to a key only this process holds, if asked), the digest that changes when the
/// values would, and a watch that follows it. It mirrors <c>@roll/secrets</c>,
/// <c>sdk/go/secrets</c> and <c>sdk/java/secrets</c>, with the .NET base class library only.
/// </summary>
/// <remarks>
/// A client holds a roll token (<c>rlt_…</c>) with a rule on one project, the operator token, or
/// the short-lived token a workload identity's login gives (<see cref="RollSecretsOptions.Identity"/>).
/// It's safe to share between threads; make one and keep it.
/// </remarks>
public sealed class RollSecretsClient : IDisposable
{
    /// <summary>The most of an answer that's read: past it, the answer is refused.</summary>
    private const int MaxBody = 16 << 20;

    /// <summary>The server holds a long poll at most this long.</summary>
    internal static readonly TimeSpan MaxWait = TimeSpan.FromSeconds(25);

    private static readonly MediaTypeHeaderValue JsonType = new("application/json");

    private readonly string _api;
    private readonly HttpClient _http;
    private readonly bool _ownsHttp;
    private readonly TimeSpan _timeout;
    private readonly TimeSpan _cacheTtl;
    private readonly TimeProvider _clock;
    private readonly Relogin? _login;
    private readonly SemaphoreSlim _whoami = new(1, 1);
    private readonly ConcurrentDictionary<string, Cached> _cache = new();
    private volatile string? _token;
    private volatile string? _project;

    /// <summary>A Values answer kept by CacheTtl, with the digest it was read against.</summary>
    private sealed record Cached(SecretValues Values, string Etag, DateTimeOffset At);

    /// <summary>What one request answered: its status, its body and its ETag.</summary>
    private sealed record Answer(HttpStatusCode Status, byte[] Body, string? Etag);

    /// <summary>A client from <paramref name="options"/>, with anything they leave null from the environment.</summary>
    public RollSecretsClient(RollSecretsOptions? options = null)
        : this(options ?? new RollSecretsOptions(), Environment.GetEnvironmentVariable)
    {
    }

    internal RollSecretsClient(RollSecretsOptions o, Func<string, string?> environment)
    {
        string? Env(string k)
        {
            var v = environment(k);
            return string.IsNullOrWhiteSpace(v) ? null : v.Trim();
        }

        var api = o.Api ?? Env("ROLL_API_URL");
        if (string.IsNullOrWhiteSpace(api))
        {
            throw new RollSecretsException(400, "config", "set Api, or ROLL_API_URL");
        }
        _api = api.TrimEnd('/');
        _token = o.Token ?? Env("ROLL_TOKEN");
        _project = o.Project ?? Env("ROLL_PROJECT");
        var identity = o.Identity ?? Env("ROLL_IDENTITY");
        if (_token is null && identity is null)
        {
            throw new RollSecretsException(400, "config", "set a Token (ROLL_TOKEN), or an Identity (ROLL_IDENTITY) for a workload identity");
        }
        if (o.Timeout <= TimeSpan.Zero)
        {
            throw new ArgumentOutOfRangeException(nameof(o.Timeout), "Timeout must be positive");
        }
        _timeout = o.Timeout;
        _cacheTtl = o.CacheTtl;
        _clock = o.TimeProvider ?? TimeProvider.System;
        if (o.HttpClient is { } http)
        {
            _http = http;
        }
        else
        {
            _http = new HttpClient(new SocketsHttpHandler { PooledConnectionLifetime = TimeSpan.FromMinutes(2), ConnectTimeout = TimeSpan.FromSeconds(10) })
            {
                Timeout = System.Threading.Timeout.InfiniteTimeSpan, // each request has its own
            };
            _ownsHttp = true;
        }
        if (_token is null)
        {
            var source = o.OidcToken ?? (ct => Oidc.TokenAsync(_http, _api, Env, ct));
            _login = new Relogin(this, identity!, source);
        }
    }

    // Requests.

    private async Task<Answer> SendAsync(string? token, HttpMethod method, string path, byte[]? body, string? ifNoneMatch, TimeSpan wait, CancellationToken ct)
    {
        using var req = new HttpRequestMessage(method, _api + path);
        if (!string.IsNullOrEmpty(token))
        {
            req.Headers.Authorization = new AuthenticationHeaderValue("Bearer", token);
        }
        if (ifNoneMatch is not null)
        {
            req.Headers.TryAddWithoutValidation("If-None-Match", "\"" + ifNoneMatch + "\"");
        }
        if (body is not null)
        {
            req.Content = new ByteArrayContent(body);
            req.Content.Headers.ContentType = JsonType;
        }
        using var cts = CancellationTokenSource.CreateLinkedTokenSource(ct);
        cts.CancelAfter(_timeout + wait);
        try
        {
            using var res = await _http.SendAsync(req, HttpCompletionOption.ResponseHeadersRead, cts.Token).ConfigureAwait(false);
            byte[] data = await ReadBoundedAsync(res, cts.Token).ConfigureAwait(false);
            string? etag = res.Headers.ETag?.Tag.Trim('"');
            int status = (int)res.StatusCode;
            if (status >= 300 && res.StatusCode != HttpStatusCode.NotModified)
            {
                string? message = null, code = null;
                try
                {
                    using var e = JsonDocument.Parse(data);
                    message = Json.Str(e.RootElement, "error");
                    code = Json.Str(e.RootElement, "code");
                }
                catch (JsonException)
                {
                    // the status says it
                }
                throw new RollSecretsException(status, code, message ?? $"HTTP {status}");
            }
            return new Answer(res.StatusCode, data, etag);
        }
        catch (OperationCanceledException) when (!ct.IsCancellationRequested)
        {
            throw new RollSecretsException(0, "timeout", "roll secrets didn't answer in time");
        }
        catch (HttpRequestException e)
        {
            throw new RollSecretsException(0, "io", "roll secrets didn't answer: " + e.Message, e);
        }
    }

    private static async Task<byte[]> ReadBoundedAsync(HttpResponseMessage res, CancellationToken ct)
    {
        using var s = await res.Content.ReadAsStreamAsync(ct).ConfigureAwait(false);
        using var buf = new MemoryStream();
        var chunk = new byte[16 << 10];
        int n;
        while ((n = await s.ReadAsync(chunk, ct).ConfigureAwait(false)) > 0)
        {
            if (buf.Length + n > MaxBody)
            {
                throw new RollSecretsException((int)res.StatusCode, "too_large", "the answer is over 16 MiB");
            }
            buf.Write(chunk, 0, n);
        }
        return buf.ToArray();
    }

    /// <summary>
    /// Runs <paramref name="request"/> with the client's token. A logged-in client's is renewed
    /// first when it's ending, and when the server refuses it: one new login, then once more.
    /// </summary>
    private async Task<Answer> AuthedAsync(Func<string?, Task<Answer>> request, CancellationToken ct)
    {
        if (_login is null)
        {
            return await request(_token).ConfigureAwait(false);
        }
        string t = await _login.TokenAsync(null, ct).ConfigureAwait(false);
        try
        {
            return await request(t).ConfigureAwait(false);
        }
        catch (RollSecretsException e) when (e.Status == 401)
        {
            return await request(await _login.TokenAsync(t, ct).ConfigureAwait(false)).ConfigureAwait(false);
        }
    }

    internal async Task<JsonDocument> CallAsync(HttpMethod method, string path, byte[]? body, CancellationToken ct, TimeSpan wait = default)
    {
        var a = await AuthedAsync(t => SendAsync(t, method, path, body, null, wait, ct), ct).ConfigureAwait(false);
        return Json.Parse(a.Body.Length == 0 ? "{}"u8.ToArray() : a.Body);
    }

    private static string Seg(string s) => Uri.EscapeDataString(s);

    private static string Query(string? path) => string.IsNullOrEmpty(path) ? "" : "?path=" + Uri.EscapeDataString(path);

    private static string OrRoot(string? path) => string.IsNullOrEmpty(path) ? "/" : path;

    /// <summary>The project: as given, or the token's own (<c>GET /v1/secrets/whoami</c>).</summary>
    public async Task<string> ProjectIdAsync(CancellationToken ct = default)
    {
        if (_project is { } p)
        {
            return p;
        }
        if (_login is not null)
        {
            await _login.TokenAsync(null, ct).ConfigureAwait(false); // a login names its identity's project
        }
        await _whoami.WaitAsync(ct).ConfigureAwait(false);
        try
        {
            if (_project is null)
            {
                using var w = await CallAsync(HttpMethod.Get, "/v1/secrets/whoami", null, ct).ConfigureAwait(false);
                var own = Json.Str(w.RootElement, "project");
                if (string.IsNullOrEmpty(own))
                {
                    throw new RollSecretsException(400, "project", "this token isn't limited to one project: set Project");
                }
                _project = own;
            }
            return _project;
        }
        finally
        {
            _whoami.Release();
        }
    }

    private async Task<string> EnvAsync(string env, CancellationToken ct) =>
        "/v1/secrets/projects/" + Seg(await ProjectIdAsync(ct).ConfigureAwait(false)) + "/environments/" + Seg(env);

    // Reading.

    /// <summary><paramref name="env"/>'s values under <paramref name="path"/> (<c>""</c> for the root), references resolved.</summary>
    public async Task<SecretValues> ValuesAsync(string env, string path = "", CancellationToken ct = default)
    {
        string url = await EnvAsync(env, ct).ConfigureAwait(false) + "/values" + Query(path);
        if (_cacheTtl <= TimeSpan.Zero)
        {
            using var d = await CallAsync(HttpMethod.Get, url, null, ct).ConfigureAwait(false);
            return SecretValues.Parse(d.RootElement);
        }
        return await CachedAsync(url, ct).ConfigureAwait(false);
    }

    /// <summary>A fresh read (a watch that saw the digest move): a kept answer for the folder goes.</summary>
    internal async Task<SecretValues> FreshValuesAsync(string env, string path, CancellationToken ct)
    {
        string url = await EnvAsync(env, ct).ConfigureAwait(false) + "/values" + Query(path);
        using var d = await CallAsync(HttpMethod.Get, url, null, ct).ConfigureAwait(false);
        var v = SecretValues.Parse(d.RootElement);
        _cache.TryRemove(url, out _);
        return v;
    }

    /// <summary>From memory within CacheTtl, else revalidated with the digest (a 304 keeps the values).</summary>
    private async Task<SecretValues> CachedAsync(string url, CancellationToken ct)
    {
        _cache.TryGetValue(url, out var hit);
        if (hit is not null && _clock.GetUtcNow() - hit.At < _cacheTtl)
        {
            return hit.Values;
        }
        string sep = url.Contains('?') ? "&" : "?";
        var a = await AuthedAsync(t => SendAsync(t, HttpMethod.Get, url + sep + "digest=1", null, hit?.Etag, TimeSpan.Zero, ct), ct).ConfigureAwait(false);
        if (a.Status == HttpStatusCode.NotModified && hit is not null)
        {
            _cache[url] = hit with { At = _clock.GetUtcNow() };
            return hit.Values;
        }
        using var d = Json.Parse(a.Body);
        var v = SecretValues.Parse(d.RootElement);
        // Never kept: dynamic credentials (they end with their lease), a key that didn't resolve (the
        // digest may not move when the link recovers), or an answer with nothing to revalidate by.
        if (v.Dynamic.Count > 0 || v.Problems.Count > 0 || string.IsNullOrEmpty(a.Etag))
        {
            _cache.TryRemove(url, out _);
        }
        else
        {
            _cache[url] = new Cached(v, a.Etag, _clock.GetUtcNow());
        }
        return v;
    }

    /// <summary>
    /// One key's value under <paramref name="path"/>, references resolved
    /// (<c>GET …/secrets/{key}/value</c>: one read, audited as such). For many keys, or a key read
    /// per request with <see cref="RollSecretsOptions.CacheTtl"/>, read <see cref="ValuesAsync"/>
    /// once instead.
    /// </summary>
    /// <exception cref="RollSecretsException">404 when there's no such key, 409 <c>unresolved</c> when it doesn't resolve.</exception>
    public async Task<string> GetAsync(string env, string path, string key, CancellationToken ct = default)
    {
        string url = await EnvAsync(env, ct).ConfigureAwait(false) + "/secrets/" + Seg(key) + "/value" + Query(path);
        using var d = await CallAsync(HttpMethod.Get, url, null, ct).ConfigureAwait(false);
        var problems = SecretValues.Parse(d.RootElement).Problems;
        if (problems.Count > 0)
        {
            throw new RollSecretsException(409, "unresolved", $"{key} doesn't resolve: {problems[0].Description}");
        }
        return Json.Str(d.RootElement, "value") ?? throw new RollSecretsException(0, "invalid", "the answer has no value");
    }

    /// <summary>
    /// <paramref name="env"/>'s values as <paramref name="consumer"/> (<c>app:web</c>), what a
    /// running service gets: dynamic credentials' leases are the consumer's, and the read is the
    /// consumer's in the audit log.
    /// </summary>
    public async Task<SecretValues> ResolveAsync(string env, string path, string consumer, CancellationToken ct = default)
    {
        var body = Json.Write(("consumer", consumer), ("path", OrRoot(path)));
        using var d = await CallAsync(HttpMethod.Post, await EnvAsync(env, ct).ConfigureAwait(false) + "/resolve", body, ct).ConfigureAwait(false);
        return SecretValues.Parse(d.RootElement);
    }

    /// <summary>
    /// <see cref="ResolveAsync"/> with the values sealed to <paramref name="key"/> on the server and
    /// opened here: they are never in the clear between the two.
    /// </summary>
    public async Task<SecretValues> ResolveSealedAsync(string env, string path, string consumer, SealKey key, CancellationToken ct = default)
    {
        ArgumentNullException.ThrowIfNull(key);
        var body = Json.Write(("consumer", consumer), ("path", OrRoot(path)), ("seal_to", key.PublicKey));
        using var d = await CallAsync(HttpMethod.Post, await EnvAsync(env, ct).ConfigureAwait(false) + "/resolve", body, ct).ConfigureAwait(false);
        var o = d.RootElement;
        if (!o.TryGetProperty("sealed", out var s) || s.ValueKind != JsonValueKind.Object)
        {
            throw new RollSecretsException(0, "invalid", "the answer isn't sealed");
        }
        var box = new Sealed((int)Json.Num(s, "v"), Json.Str(s, "epk") ?? "", Json.Str(s, "nonce") ?? "", Json.Str(s, "ct") ?? "");
        byte[] plain;
        try
        {
            plain = key.Open(box, Json.Str(o, "context") ?? "");
        }
        catch (CryptographicException)
        {
            throw new RollSecretsException(0, "sealed", "the sealed values don't open with this key");
        }
        try
        {
            using var values = Json.Parse(plain, "the sealed values aren't a map");
            if (values.RootElement.ValueKind != JsonValueKind.Object)
            {
                throw new RollSecretsException(0, "invalid", "the sealed values aren't a map");
            }
            return SecretValues.Parse(o, Json.StringMap(values.RootElement));
        }
        finally
        {
            CryptographicOperations.ZeroMemory(plain);
        }
    }

    /// <summary>Moves whenever <paramref name="env"/>'s values under <paramref name="path"/> would; it reads no value.</summary>
    public Task<string> DigestAsync(string env, string path = "", CancellationToken ct = default) =>
        DigestSinceAsync(env, path, null, TimeSpan.Zero, ct);

    /// <summary>
    /// A long poll of the digest: it answers as soon as it differs from <paramref name="since"/>, or
    /// with the same one once <paramref name="wait"/> (at most 25 s, the server's limit) runs out. A
    /// server without long polls answers at once. No <paramref name="since"/>, or a wait under a
    /// second: an ordinary read.
    /// </summary>
    public async Task<string> DigestSinceAsync(string env, string path, string? since, TimeSpan wait, CancellationToken ct = default)
    {
        string q = Query(path);
        TimeSpan held = TimeSpan.Zero;
        if (!string.IsNullOrEmpty(since) && wait >= TimeSpan.FromSeconds(1))
        {
            held = wait > MaxWait ? MaxWait : wait;
            q += (q.Length == 0 ? "?" : "&") + "since=" + Uri.EscapeDataString(since) + "&wait=" + (long)held.TotalSeconds;
        }
        using var d = await CallAsync(HttpMethod.Get, await EnvAsync(env, ct).ConfigureAwait(false) + "/digest" + q, null, ct, held).ConfigureAwait(false);
        return Json.Str(d.RootElement, "digest") ?? throw new RollSecretsException(0, "invalid", "the answer has no digest");
    }

    /// <summary>Pushes a dynamic lease's expiry out by <paramref name="ttlMinutes"/> (0: its link's TTL).</summary>
    public async Task<Lease> RenewAsync(string lease, int ttlMinutes = 0, CancellationToken ct = default)
    {
        var body = Json.Write(("ttl_minutes", ttlMinutes > 0 ? ttlMinutes : null));
        string url = "/v1/secrets/projects/" + Seg(await ProjectIdAsync(ct).ConfigureAwait(false)) + "/leases/" + Seg(lease) + "/renew";
        using var d = await CallAsync(HttpMethod.Post, url, body, ct).ConfigureAwait(false);
        return Lease.Parse(d.RootElement);
    }

    // Encryption keys.

    /// <summary>
    /// The project's encryption key <paramref name="name"/> (docs/secrets.md, "Encryption keys"):
    /// data is encrypted, signed and HMACed by roll, and the key never leaves it. It asks roll
    /// nothing until it's used.
    /// </summary>
    public Transit Transit(string name)
    {
        ArgumentException.ThrowIfNullOrEmpty(name);
        return new Transit(this, name);
    }

    // Watching.

    /// <summary>
    /// Calls <see cref="WatchOptions.OnChange"/> with <paramref name="env"/>'s values now, then
    /// again whenever its digest moves, renewing the dynamic leases of the latest values meanwhile
    /// (as <c>roll run --watch</c> does), until the watch is disposed. It long-polls, so a change
    /// arrives within a second or two; a server that answers at once is asked again every
    /// <see cref="WatchOptions.Every"/>.
    /// </summary>
    public SecretsWatch Watch(string env, WatchOptions options)
    {
        ArgumentNullException.ThrowIfNull(options);
        ArgumentNullException.ThrowIfNull(options.OnChange);
        if (options.Every <= TimeSpan.Zero || options.RenewEvery <= TimeSpan.Zero)
        {
            throw new ArgumentOutOfRangeException(nameof(options), "Every and RenewEvery must be positive");
        }
        return new SecretsWatch(this, env, options);
    }

    /// <summary><see cref="Watch(string, WatchOptions)"/> at the root with the defaults.</summary>
    public SecretsWatch Watch(string env, Action<SecretValues> onChange) => Watch(env, new WatchOptions { OnChange = onChange });

    internal Task<SecretValues> LoadAsync(string env, WatchOptions o, CancellationToken ct) =>
        o.Consumer is { } consumer ? ResolveAsync(env, o.Path, consumer, ct) : FreshValuesAsync(env, o.Path, ct);

    // Workload identities.

    /// <summary>
    /// Keeps a logged-in client's token fresh: a new login once less than a fifth of its life (at
    /// least a minute) is left, or when the server refuses it.
    /// </summary>
    private sealed class Relogin(RollSecretsClient c, string identity, Func<CancellationToken, Task<string>> source)
    {
        private readonly SemaphoreSlim _gate = new(1, 1);
        private DateTimeOffset _issued;
        private DateTimeOffset? _expires;

        /// <summary>The client's token, renewed first when it's about to end (or <paramref name="stale"/>: refused).</summary>
        public async Task<string> TokenAsync(string? stale, CancellationToken ct)
        {
            await _gate.WaitAsync(ct).ConfigureAwait(false);
            try
            {
                var now = DateTimeOffset.UtcNow;
                if (c._token is null)
                {
                    await RenewAsync(ct).ConfigureAwait(false);
                    return c._token!;
                }
                bool refused = stale is not null && stale == c._token;
                bool ending = false;
                if (_expires is { } expires)
                {
                    var margin = (expires - _issued) / 5;
                    ending = now > expires - (margin < TimeSpan.FromMinutes(1) ? TimeSpan.FromMinutes(1) : margin);
                }
                if (refused || ending)
                {
                    try
                    {
                        await RenewAsync(ct).ConfigureAwait(false);
                    }
                    catch (RollSecretsException) when (!refused && now < _expires)
                    {
                        // Renewing early that fails keeps the token until it really ends (the
                        // identity provider or roll may be down for a moment).
                    }
                }
                return c._token!;
            }
            finally
            {
                _gate.Release();
            }
        }

        private async Task RenewAsync(CancellationToken ct)
        {
            string oidc = await source(ct).ConfigureAwait(false) ?? "";
            var body = Json.Write(("identity", identity), ("token", oidc.Trim()));
            var a = await c.SendAsync(null, HttpMethod.Post, "/v1/secrets/login", body, null, TimeSpan.Zero, ct).ConfigureAwait(false);
            using var d = Json.Parse(a.Body);
            var t = Json.Str(d.RootElement, "token");
            if (string.IsNullOrEmpty(t))
            {
                throw new RollSecretsException(0, "invalid", "the login answered no token");
            }
            c._token = t;
            _issued = DateTimeOffset.UtcNow;
            long expires = Json.Num(d.RootElement, "expires_at");
            // An answer without an expiry is renewed only when the server refuses it.
            _expires = expires > 0 ? DateTimeOffset.FromUnixTimeMilliseconds(expires) : null;
            if (c._project is null && Json.Str(d.RootElement, "project") is { Length: > 0 } p)
            {
                c._project = p;
            }
        }
    }

    /// <summary>Forgets kept answers and, if the client made its own HTTP client, closes it.</summary>
    public void Dispose()
    {
        _cache.Clear();
        if (_ownsHttp)
        {
            _http.Dispose();
        }
    }

    /// <summary>Names the API and the project, never the token.</summary>
    public override string ToString() => $"RollSecretsClient {{ {_api}{(_project is { } p ? ", " + p : "")} }}";
}
