using System.Net;
using System.Text;
using System.Text.Json;

namespace Roll.Secrets.Tests;

/// <summary>rolld as the client sees it, in memory: one handler answers every request.</summary>
internal sealed class FakeRolld : HttpMessageHandler
{
    public const string Url = "http://rolld.test";

    /// <summary>One request.</summary>
    public sealed class Request
    {
        public required string Method { get; init; }
        public required string Path { get; init; }
        public required Dictionary<string, string> Query { get; init; }
        public required string Body { get; init; }
        public required HttpRequestMessage Message { get; init; }

        public string? Header(string name) => Message.Headers.TryGetValues(name, out var v) ? string.Join(",", v) : null;

        public JsonElement Json() => JsonDocument.Parse(Body).RootElement;
    }

    /// <summary>An answer: a status, a JSON body (or none) and headers.</summary>
    public sealed record Response(int Status, object? Body = null, Dictionary<string, string>? Headers = null);

    /// <summary>A body sent as it is, not as JSON.</summary>
    public sealed record Raw(string Text);

    private readonly Func<Request, CancellationToken, Task<Response>> _handle;
    public readonly List<string> Seen = new();

    public FakeRolld(Func<Request, CancellationToken, Task<Response>> handle) => _handle = handle;

    public FakeRolld(Func<Request, Response> handle) : this((r, _) => Task.FromResult(handle(r))) { }

    public HttpClient Client() => new(this) { Timeout = Timeout.InfiniteTimeSpan };

    protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage req, CancellationToken ct)
    {
        var uri = req.RequestUri!;
        var query = new Dictionary<string, string>();
        foreach (var part in uri.Query.TrimStart('?').Split('&', StringSplitOptions.RemoveEmptyEntries))
        {
            int eq = part.IndexOf('=');
            query[Uri.UnescapeDataString(eq < 0 ? part : part[..eq])] = eq < 0 ? "" : Uri.UnescapeDataString(part[(eq + 1)..]);
        }
        lock (Seen)
        {
            Seen.Add(req.Method + " " + uri.PathAndQuery);
        }
        var r = new Request
        {
            Method = req.Method.Method,
            Path = uri.AbsolutePath,
            Query = query,
            Body = req.Content is null ? "" : await req.Content.ReadAsStringAsync(ct),
            Message = req,
        };
        var a = await _handle(r, ct);
        var res = new HttpResponseMessage((HttpStatusCode)a.Status)
        {
            Content = a.Body switch
            {
                null => new ByteArrayContent(Array.Empty<byte>()),
                Raw raw => new StringContent(raw.Text, Encoding.UTF8, "application/json"),
                var body => new StringContent(JsonSerializer.Serialize(body), Encoding.UTF8, "application/json"),
            },
            RequestMessage = req,
        };
        foreach (var (k, v) in a.Headers ?? new())
        {
            res.Headers.TryAddWithoutValidation(k, v);
        }
        return res;
    }

    public static object Values(Dictionary<string, string> values, string[]? dynamic = null, object[]? problems = null) => new Dictionary<string, object>
    {
        ["values"] = values,
        ["problems"] = problems ?? Array.Empty<object>(),
        ["leases"] = Array.Empty<string>(),
        ["dynamic"] = dynamic ?? Array.Empty<string>(),
        ["linked"] = Array.Empty<string>(),
    };

    public static Response Error(int status, string error, string? code = null) =>
        new(status, code is null ? new Dictionary<string, string> { ["error"] = error } : new Dictionary<string, string> { ["error"] = error, ["code"] = code });
}

/// <summary>A clock a test moves by hand.</summary>
internal sealed class FakeClock : TimeProvider
{
    public DateTimeOffset Now = DateTimeOffset.FromUnixTimeSeconds(1_700_000_000);

    public override DateTimeOffset GetUtcNow() => Now;
}

/// <summary>A value a test changes, and a long poll's wait for it to change.</summary>
internal sealed class Changing<T>(T initial)
{
    private readonly object _lock = new();
    private T _value = initial;
    private TaskCompletionSource _moved = new(TaskCreationOptions.RunContinuationsAsynchronously);

    public T Value
    {
        get
        {
            lock (_lock)
            {
                return _value;
            }
        }
    }

    public void Set(T value)
    {
        TaskCompletionSource moved;
        lock (_lock)
        {
            _value = value;
            moved = _moved;
            _moved = new(TaskCreationOptions.RunContinuationsAsynchronously);
        }
        moved.SetResult();
    }

    /// <summary>Waits until the value is no longer <paramref name="unless"/>'s, or <paramref name="wait"/> runs out.</summary>
    public async Task<T> WaitWhileAsync(Func<T, bool> unless, TimeSpan wait, CancellationToken ct)
    {
        var until = DateTime.UtcNow + wait;
        while (true)
        {
            Task moved;
            lock (_lock)
            {
                if (!unless(_value) || DateTime.UtcNow >= until)
                {
                    return _value;
                }
                moved = _moved.Task;
            }
            var left = until - DateTime.UtcNow;
            if (left > TimeSpan.Zero)
            {
                await Task.WhenAny(moved, Task.Delay(left, ct));
                ct.ThrowIfCancellationRequested();
            }
        }
    }
}
