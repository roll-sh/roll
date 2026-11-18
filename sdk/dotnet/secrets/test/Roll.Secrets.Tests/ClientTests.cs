using System.Collections.Concurrent;
using System.Diagnostics;
using Microsoft.Extensions.Configuration;
using Roll.Secrets.Configuration;

namespace Roll.Secrets.Tests;

public class ClientTests
{
    private const string Env = "/v1/secrets/projects/prj_a/environments/production";

    private static RollSecretsClient Client(FakeRolld rolld, Action<RollSecretsOptions>? set = null, Dictionary<string, string>? env = null)
    {
        var o = new RollSecretsOptions { HttpClient = rolld.Client() };
        set?.Invoke(o);
        return new RollSecretsClient(o, k => env is not null && env.TryGetValue(k, out var v) ? v : null);
    }

    private static async Task<T> Next<T>(BlockingCollection<T> q, int ms = 5000) =>
        await Task.Run(() => q.TryTake(out var v, ms) ? v : throw new TimeoutException("nothing arrived"));

    /// <summary>
    /// A workload identity's login, a sealed resolve, a watch whose long poll brings a change at
    /// once, and a refused roll token that the client replaces with a new login.
    /// </summary>
    [Fact]
    public async Task AWorkloadLogsInReadsSealedValuesAndWatchesForChanges()
    {
        var state = new Changing<(string Digest, string Value, string Token)>(("d1", "one", "rlt_fake_workload"));
        int logins = 0;
        using var rolld = new FakeRolld(async (r, ct) =>
        {
            if (r.Path == "/v1/secrets/login")
            {
                var b = r.Json();
                if (b.GetProperty("identity").GetString() != "idn_ci" || b.GetProperty("token").GetString() != "the-oidc-token")
                {
                    return FakeRolld.Error(401, "no identity idn_ci takes this token", "unauthorized");
                }
                Interlocked.Increment(ref logins);
                return new(200, new Dictionary<string, object> { ["token"] = state.Value.Token, ["project"] = "prj_a", ["expires_at"] = DateTimeOffset.UtcNow.AddHours(1).ToUnixTimeMilliseconds() });
            }
            if (r.Header("Authorization") != "Bearer " + state.Value.Token)
            {
                return FakeRolld.Error(401, "unauthorized");
            }
            switch (r.Path)
            {
                case Env + "/digest":
                    var since = r.Query.GetValueOrDefault("since");
                    var wait = TimeSpan.FromSeconds(int.Parse(r.Query.GetValueOrDefault("wait", "0")));
                    // A long poll: held until the digest moves or the wait runs out.
                    var s = await state.WaitWhileAsync(x => since is not null && x.Digest == since, wait, ct);
                    return new(200, new Dictionary<string, string> { ["digest"] = s.Digest });
                case Env + "/values":
                    return new(200, FakeRolld.Values(new() { ["A"] = state.Value.Value }));
                case Env + "/resolve":
                    var body = r.Json();
                    using (var k = SealTests.Vector())
                    {
                        if (body.GetProperty("consumer").GetString() != "app:web" || body.GetProperty("seal_to").GetString() != k.PublicKey)
                        {
                            return FakeRolld.Error(400, "bad resolve");
                        }
                    }
                    var box = SealTests.VectorBox;
                    return new(200, new Dictionary<string, object>
                    {
                        ["sealed"] = new Dictionary<string, object> { ["v"] = 1, ["epk"] = box.Epk, ["nonce"] = box.Nonce, ["ct"] = box.Ct },
                        ["context"] = SealTests.VectorContext,
                        ["problems"] = Array.Empty<object>(),
                        ["leases"] = Array.Empty<string>(),
                        ["dynamic"] = Array.Empty<string>(),
                        ["linked"] = new[] { "DB_URL" },
                    });
                default:
                    return FakeRolld.Error(404, "not found");
            }
        });

        var env = new Dictionary<string, string> { ["ROLL_API_URL"] = FakeRolld.Url, ["ROLL_IDENTITY"] = "idn_ci", ["ROLL_OIDC_TOKEN"] = "wrong" };
        var refused = await Assert.ThrowsAsync<RollSecretsException>(() => Client(rolld, env: env).ValuesAsync("production"));
        Assert.Equal(401, refused.Status);
        Assert.Equal("unauthorized", refused.Code);

        env["ROLL_OIDC_TOKEN"] = "the-oidc-token";
        using var c = Client(rolld, env: env);
        using (var key = SealTests.Vector())
        {
            var sealedValues = await c.ResolveSealedAsync("production", "", "app:web", key);
            Assert.Equal("postgres://db/shop", sealedValues["DB_URL"]);
            Assert.Equal(new[] { "DB_URL" }, sealedValues.Linked);
        }

        var seen = new BlockingCollection<string>();
        var errors = new ConcurrentQueue<Exception>();
        // `Every` is long: only the long poll can bring a change in time.
        await using (var w = c.Watch("production", new WatchOptions { OnChange = v => seen.Add(v["A"]), Every = TimeSpan.FromSeconds(30), OnError = errors.Enqueue }))
        {
            Assert.Equal("one", await Next(seen));
            await Task.Delay(100); // the long poll is held now
            var changed = Stopwatch.StartNew();
            state.Set(("d2", "two", state.Value.Token));
            Assert.Equal("two", await Next(seen));
            Assert.True(changed.Elapsed < TimeSpan.FromSeconds(5), $"the long poll answered as the digest moved, not after Every: {changed.Elapsed}");

            // The roll token stops working (revoked, ended): the client logs in again and keeps watching.
            state.Set(("d3", "three", "rlt_fake_workload2"));
            Assert.Equal("three", await Next(seen));
            Assert.Equal(2, logins);
            Assert.False(seen.TryTake(out _, 200), "an unchanged digest delivered again");
            Assert.True(w.Running);
        }
        // Stopped: a change reaches nobody.
        state.Set(("d4", "four", state.Value.Token));
        Assert.False(seen.TryTake(out _, 300));
    }

    /// <summary>A server without long polls answers at once: the watch asks again every `Every`, never spinning.</summary>
    [Fact]
    public async Task AWatchPollsAServerThatAnswersAtOnce()
    {
        int digest = 1, digests = 0;
        using var rolld = new FakeRolld(r =>
        {
            if (r.Path == Env + "/digest")
            {
                Interlocked.Increment(ref digests);
                return new(200, new Dictionary<string, string> { ["digest"] = "d" + Volatile.Read(ref digest) });
            }
            if (r.Path == Env + "/values")
            {
                return Volatile.Read(ref digest) == 2
                    ? new(200, FakeRolld.Values(new(), problems: new object[] { new Dictionary<string, string> { ["key"] = "A", ["problem"] = "no such key B" } }))
                    : new(200, FakeRolld.Values(new() { ["A"] = "v" + Volatile.Read(ref digest) }));
            }
            return FakeRolld.Error(404, "not found");
        });
        using var c = Client(rolld, o => { o.Api = FakeRolld.Url; o.Token = "rlt_fake"; o.Project = "prj_a"; });
        var seen = new BlockingCollection<string>();
        var errors = new BlockingCollection<Exception>();
        using (var w = c.Watch("production", new WatchOptions { OnChange = v => seen.Add(v["A"]), Every = TimeSpan.FromMilliseconds(20), OnError = errors.Add }))
        {
            Assert.Equal("v1", await Next(seen));
            // A change that doesn't resolve keeps what's running and says why.
            Volatile.Write(ref digest, 2);
            var e = Assert.IsType<RollSecretsException>(await Next(errors));
            Assert.Equal(409, e.Status);
            Assert.Equal("unresolved", e.Code);
            Volatile.Write(ref digest, 3);
            Assert.Equal("v3", await Next(seen));
            int before = Volatile.Read(ref digests);
            await Task.Delay(300);
            int asked = Volatile.Read(ref digests) - before;
            Assert.True(asked >= 2 && asked <= 20, $"paced by Every, not spinning: {asked} in 300 ms");
            Assert.True(w.Running);
        }
        lock (rolld.Seen)
        {
            Assert.Contains(rolld.Seen, s => s.Contains("since=d1&wait=25"));
        }
    }

    /// <summary>
    /// Values are kept for CacheTtl, revalidated with their digest (a 304 keeps them), and never
    /// kept when they hold dynamic credentials or come without an ETag.
    /// </summary>
    [Fact]
    public async Task ValuesAreCachedAndRevalidatedWithTheirDigest()
    {
        string digest = "d1", value = "1";
        bool etag = true;
        var dynamic = new List<string>();
        var asked = new List<string>();
        using var rolld = new FakeRolld(r =>
        {
            var tag = r.Header("If-None-Match");
            lock (asked)
            {
                asked.Add(tag ?? "none");
            }
            Assert.Equal("1", r.Query["digest"]);
            var headers = etag ? new Dictionary<string, string> { ["ETag"] = "\"" + digest + "\"" } : null;
            if (tag == "\"" + digest + "\"")
            {
                return new(304, null, headers);
            }
            return new(200, FakeRolld.Values(new() { ["A"] = value }, dynamic.ToArray()), headers);
        });
        var clock = new FakeClock();
        using var c = Client(rolld, o => { o.Api = FakeRolld.Url; o.Token = "rlt_fake"; o.Project = "prj_a"; o.CacheTtl = TimeSpan.FromSeconds(20); o.TimeProvider = clock; });

        Assert.Equal("1", (await c.ValuesAsync("production"))["A"]);
        Assert.Equal("1", (await c.ValuesAsync("production"))["A"]);
        Assert.Equal(new[] { "none" }, asked); // within the TTL, from memory

        clock.Now += TimeSpan.FromSeconds(25);
        Assert.Equal("1", (await c.ValuesAsync("production"))["A"]);
        Assert.Equal(new[] { "none", "\"d1\"" }, asked); // revalidated: a 304

        clock.Now += TimeSpan.FromSeconds(25);
        (digest, value) = ("d2", "2");
        dynamic.Add("PW");
        Assert.Equal("2", (await c.ValuesAsync("production"))["A"]);
        Assert.Equal("2", (await c.ValuesAsync("production"))["A"]);
        Assert.Equal(4, asked.Count); // a change, then dynamic credentials read every time

        dynamic.Clear();
        etag = false;
        await c.ValuesAsync("production");
        await c.ValuesAsync("production");
        Assert.Equal(6, asked.Count); // nothing to revalidate by: never kept
    }

    /// <summary>One key, errors with roll's code, and nothing printable that holds a value.</summary>
    [Fact]
    public async Task GetReadsOneKeyAndNothingPrintsAValue()
    {
        const string fake = "fake-value-123";
        using var rolld = new FakeRolld(r => r.Path switch
        {
            "/v1/secrets/whoami" => new(200, new Dictionary<string, string> { ["project"] = "prj_a" }),
            Env + "/secrets/API_KEY/value" when r.Query["path"] == "/svc" => new(200, new Dictionary<string, object> { ["value"] = fake, ["problems"] = Array.Empty<object>() }),
            Env + "/secrets/BROKEN/value" => new(200, new Dictionary<string, object> { ["value"] = "", ["problems"] = new[] { new Dictionary<string, string> { ["key"] = "BROKEN", ["problem"] = "a reference to nothing" } } }),
            Env + "/values" => new(200, FakeRolld.Values(new() { ["API_KEY"] = fake })),
            Env + "/resolve" => new(200, new Dictionary<string, object> { ["sealed"] = new Dictionary<string, object> { ["v"] = 1, ["epk"] = "x", ["nonce"] = "y", ["ct"] = fake } }),
            Env + "/digest" => new(200, null, null),
            _ => FakeRolld.Error(403, "this token can't read production", "forbidden"),
        });
        using var c = Client(rolld, o => { o.Api = FakeRolld.Url + "/"; o.Token = "rlt_fake_token"; });
        Assert.Equal(fake, await c.GetAsync("production", "/svc", "API_KEY"));
        Assert.Equal("prj_a", await c.ProjectIdAsync());

        var unresolved = await Assert.ThrowsAsync<RollSecretsException>(() => c.GetAsync("production", "", "BROKEN"));
        Assert.Equal("unresolved", unresolved.Code);
        var forbidden = await Assert.ThrowsAsync<RollSecretsException>(() => c.DigestAsync("staging"));
        Assert.Equal(403, forbidden.Status);
        Assert.Equal("forbidden", forbidden.Code);
        Assert.Equal("this token can't read production", forbidden.Message);
        var empty = await Assert.ThrowsAsync<RollSecretsException>(() => c.DigestAsync("production"));
        Assert.Equal("invalid", empty.Code);
        using var key = SealKey.Generate();
        var unopened = await Assert.ThrowsAsync<RollSecretsException>(() => c.ResolveSealedAsync("production", "", "app:web", key));
        Assert.DoesNotContain(fake, unopened.ToString());

        var v = await c.ValuesAsync("production");
        Assert.Equal(fake, v["API_KEY"]);
        Assert.Contains("API_KEY", v.ToString());
        Assert.DoesNotContain(fake, v.ToString());
        var missing = Assert.Throws<KeyNotFoundException>(() => v["NOPE"]);
        Assert.DoesNotContain(fake, missing.Message);
        Assert.DoesNotContain("rlt_fake_token", c.ToString());
        Assert.DoesNotContain("rlt_fake_token", new RollSecretsOptions { Token = "rlt_fake_token" }.ToString());
        Assert.Throws<NotSupportedException>(() => ((IDictionary<string, string>)v.Values).Add("B", "x"));
    }

    [Fact]
    public async Task AnAnswerThatIsNotJsonNeverQuotesItself()
    {
        using var rolld = new FakeRolld(_ => new(200, new FakeRolld.Raw("{\"values\":{\"A\":\"fake-value-123\"")));
        using var c = Client(rolld, o => { o.Api = FakeRolld.Url; o.Token = "rlt_fake"; o.Project = "prj_a"; });
        var e = await Assert.ThrowsAsync<RollSecretsException>(() => c.ValuesAsync("production"));
        Assert.Equal("invalid", e.Code);
        Assert.DoesNotContain("fake-value", e.ToString());
    }

    /// <summary>AddRollSecrets: values as configuration (`__` a section), reloaded when the environment changes.</summary>
    [Fact]
    public async Task ConfigurationLoadsValuesAndReloadsOnChange()
    {
        var state = new Changing<(string Digest, string Value)>(("d1", "one"));
        using var rolld = new FakeRolld(async (r, ct) =>
        {
            if (r.Path == Env + "/digest")
            {
                var since = r.Query.GetValueOrDefault("since");
                var s = await state.WaitWhileAsync(x => since is not null && x.Digest == since, TimeSpan.FromSeconds(int.Parse(r.Query.GetValueOrDefault("wait", "0"))), ct);
                return new(200, new Dictionary<string, string> { ["digest"] = s.Digest });
            }
            return new(200, FakeRolld.Values(new() { ["DB__HOST"] = state.Value.Value, ["PLAIN"] = "fake" }));
        });
        var root = new ConfigurationBuilder()
            .AddRollSecrets("production", s =>
            {
                s.Client = new RollSecretsOptions { Api = FakeRolld.Url, Token = "rlt_fake", Project = "prj_a", HttpClient = rolld.Client() };
                s.ReloadOnChange = true;
                s.Every = TimeSpan.FromSeconds(30);
            })
            .Build();
        try
        {
            Assert.Equal("one", root["DB:HOST"]);
            Assert.Equal("one", root.GetSection("db")["host"]);
            Assert.Equal("fake", root["PLAIN"]);

            var reloaded = new TaskCompletionSource();
            using var _ = root.GetReloadToken().RegisterChangeCallback(_ => reloaded.TrySetResult(), null);
            await Task.Delay(100);
            state.Set(("d2", "two"));
            await reloaded.Task.WaitAsync(TimeSpan.FromSeconds(5));
            Assert.Equal("two", root["DB:HOST"]);
        }
        finally
        {
            ((IDisposable)root).Dispose();
        }

        var optional = new ConfigurationBuilder().AddRollSecrets("production", s =>
        {
            s.Client = new RollSecretsOptions { Api = FakeRolld.Url, Token = "rlt_fake", Project = "prj_a", HttpClient = new FakeRolld(_ => FakeRolld.Error(403, "no", "forbidden")).Client() };
            s.Optional = true;
        }).Build();
        Assert.Null(optional["DB:HOST"]);
    }
}
