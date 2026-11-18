using System.Collections.ObjectModel;
using System.Text.Json;

namespace Roll.Secrets;

/// <summary>
/// An answer that wasn't a success: its HTTP status, roll's code (<c>forbidden</c>,
/// <c>unresolved</c>…) and message. It never holds a value. A request that never got an answer
/// has status 0 and code <c>io</c> or <c>timeout</c>; a client set up wrong, 400 and <c>config</c>.
/// </summary>
public class RollSecretsException : Exception
{
    /// <summary>An error with roll's status, code and message.</summary>
    public RollSecretsException(int status, string? code, string message, Exception? inner = null)
        : base(message, inner)
    {
        Status = status;
        Code = code;
    }

    /// <summary>The HTTP status, or 0 when there was no answer.</summary>
    public int Status { get; }

    /// <summary>roll's code for the error, or null when the answer named none.</summary>
    public string? Code { get; }
}

/// <summary>A key that didn't resolve (a reference to nothing, a cycle, a link that failed).</summary>
public sealed record Problem(string Key, string Description);

/// <summary>A dynamic credential's lease. <see cref="RevokedAt"/> is null while it lives.</summary>
public sealed record Lease(
    string Id,
    string Environment,
    string Path,
    string Key,
    string Kind,
    string? Consumer,
    DateTimeOffset IssuedAt,
    DateTimeOffset ExpiresAt,
    DateTimeOffset? RevokedAt)
{
    internal static Lease Parse(JsonElement o)
    {
        DateTimeOffset At(string name) => DateTimeOffset.FromUnixTimeMilliseconds(Json.Num(o, name));
        DateTimeOffset? revoked = o.TryGetProperty("revoked_at", out var r) && r.ValueKind == JsonValueKind.Number
            ? DateTimeOffset.FromUnixTimeMilliseconds(r.GetInt64())
            : null;
        return new Lease(Json.Str(o, "id") ?? "", Json.Str(o, "environment") ?? "", Json.Str(o, "path") ?? "",
            Json.Str(o, "key") ?? "", Json.Str(o, "kind") ?? "", Json.Str(o, "consumer"), At("issued_at"), At("expires_at"), revoked);
    }
}

/// <summary>
/// An environment's values. Dynamic keys hold a short-lived credential that ends with its lease
/// (<see cref="Leases"/>; <see cref="RollSecretsClient.RenewAsync"/> keeps it); linked keys come
/// from a link to a resource. It can't be changed, and <see cref="ToString"/> names keys, never
/// values.
/// </summary>
public sealed class SecretValues
{
    internal SecretValues(IDictionary<string, string> values, IList<Problem> problems, IList<string> leases, IList<string> dynamic, IList<string> linked)
    {
        Values = new ReadOnlyDictionary<string, string>(new Dictionary<string, string>(values));
        Problems = problems.ToArray();
        Leases = leases.ToArray();
        Dynamic = dynamic.ToArray();
        Linked = linked.ToArray();
    }

    /// <summary>An answer's values, or <paramref name="plain"/> in place of its <c>values</c> (a sealed resolve's, opened).</summary>
    internal static SecretValues Parse(JsonElement o, IDictionary<string, string>? plain = null)
    {
        var values = plain ?? (o.TryGetProperty("values", out var v) ? Json.StringMap(v) : new Dictionary<string, string>());
        var problems = new List<Problem>();
        if (o.TryGetProperty("problems", out var ps) && ps.ValueKind == JsonValueKind.Array)
        {
            foreach (var p in ps.EnumerateArray())
            {
                problems.Add(new Problem(Json.Str(p, "key") ?? "", Json.Str(p, "problem") ?? ""));
            }
        }
        return new SecretValues(values, problems, Json.Strings(o, "leases"), Json.Strings(o, "dynamic"), Json.Strings(o, "linked"));
    }

    /// <summary>Every key's value.</summary>
    public IReadOnlyDictionary<string, string> Values { get; }

    /// <summary>One key's value; throws <see cref="KeyNotFoundException"/> (naming the key) when there's none.</summary>
    public string this[string key] => Values.TryGetValue(key, out var v) ? v : throw new KeyNotFoundException($"no key {key}");

    /// <summary>One key's value, or null when there's none.</summary>
    public string? Get(string key) => Values.TryGetValue(key, out var v) ? v : null;

    /// <summary>Keys that didn't resolve.</summary>
    public IReadOnlyList<Problem> Problems { get; }

    /// <summary>The leases of the dynamic credentials issued for this answer.</summary>
    public IReadOnlyList<string> Leases { get; }

    /// <summary>Keys that hold a dynamic credential.</summary>
    public IReadOnlyList<string> Dynamic { get; }

    /// <summary>Keys that come from a link to a resource.</summary>
    public IReadOnlyList<string> Linked { get; }

    /// <summary>Names keys, never values.</summary>
    public override string ToString() =>
        $"SecretValues {{ Keys = [{string.Join(", ", Values.Keys)}], Problems = {Problems.Count}, Leases = [{string.Join(", ", Leases)}], Dynamic = [{string.Join(", ", Dynamic)}], Linked = [{string.Join(", ", Linked)}] }}";
}

/// <summary>The few JSON shapes the client reads and writes, without reflection.</summary>
internal static class Json
{
    /// <summary>Parses an answer. A parser's own message can quote the input, so it never reaches the caller.</summary>
    public static JsonDocument Parse(ReadOnlyMemory<byte> data, string what = "the answer isn't JSON")
    {
        try
        {
            return JsonDocument.Parse(data, new JsonDocumentOptions { MaxDepth = 64 });
        }
        catch (JsonException)
        {
            throw new RollSecretsException(0, "invalid", what);
        }
    }

    public static string? Str(JsonElement o, string name) =>
        o.ValueKind == JsonValueKind.Object && o.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;

    public static long Num(JsonElement o, string name) =>
        o.ValueKind == JsonValueKind.Object && o.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.Number && v.TryGetInt64(out var n) ? n : 0;

    public static List<string> Strings(JsonElement o, string name)
    {
        var out_ = new List<string>();
        if (o.ValueKind == JsonValueKind.Object && o.TryGetProperty(name, out var a) && a.ValueKind == JsonValueKind.Array)
        {
            foreach (var s in a.EnumerateArray())
            {
                if (s.ValueKind == JsonValueKind.String)
                {
                    out_.Add(s.GetString()!);
                }
            }
        }
        return out_;
    }

    public static Dictionary<string, string> StringMap(JsonElement o)
    {
        var map = new Dictionary<string, string>();
        if (o.ValueKind == JsonValueKind.Object)
        {
            foreach (var p in o.EnumerateObject())
            {
                if (p.Value.ValueKind == JsonValueKind.String)
                {
                    map[p.Name] = p.Value.GetString()!;
                }
            }
        }
        return map;
    }

    /// <summary>An object of strings and numbers.</summary>
    public static byte[] Write(params (string Name, object? Value)[] fields)
    {
        using var buf = new MemoryStream();
        using (var w = new Utf8JsonWriter(buf))
        {
            w.WriteStartObject();
            foreach (var (name, value) in fields)
            {
                switch (value)
                {
                    case null:
                        break;
                    case string s:
                        w.WriteString(name, s);
                        break;
                    case int i:
                        w.WriteNumber(name, i);
                        break;
                    case long l:
                        w.WriteNumber(name, l);
                        break;
                    default:
                        throw new ArgumentException($"can't write {value.GetType().Name}");
                }
            }
            w.WriteEndObject();
        }
        return buf.ToArray();
    }
}
