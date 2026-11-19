using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace Roll.Secrets;

/// <summary>
/// One of the project's encryption keys (docs/secrets.md, "Encryption keys"): data is encrypted,
/// signed and HMACed by roll, and the key never leaves it. It mirrors <c>@roll/secrets</c>'
/// <c>transit(name)</c>, <c>sdk/go/secrets</c>' <c>Transit</c> and <c>roll_secrets</c>'
/// <c>transit</c>: bytes or text (sent as UTF-8) in, base64 on the wire, bytes back from
/// <see cref="DecryptAsync"/>. Get one with <see cref="RollSecretsClient.Transit"/>.
/// </summary>
/// <remarks>
/// Ciphertexts and signatures are <c>roll:v&lt;version&gt;:…</c>: they name the key version that made
/// them, so they keep working after a rotation. A context (a row's id, say) given to
/// <see cref="EncryptAsync(ReadOnlyMemory{byte}, string?, CancellationToken)"/> is bound in, and the
/// same context must be given to decrypt. Errors are <see cref="RollSecretsException"/>s and never
/// quote what was sent.
/// </remarks>
public sealed class Transit
{
    private static readonly UTF8Encoding StrictUtf8 = new(encoderShouldEmitUTF8Identifier: false, throwOnInvalidBytes: true);

    private readonly RollSecretsClient _client;

    internal Transit(RollSecretsClient client, string name)
    {
        _client = client;
        Name = name;
    }

    /// <summary>The key's name.</summary>
    public string Name { get; }

    private async Task<string> AtAsync(string op, CancellationToken ct) =>
        "/v1/secrets/projects/" + Uri.EscapeDataString(await _client.ProjectIdAsync(ct).ConfigureAwait(false))
        + "/transit/" + Uri.EscapeDataString(Name) + "/" + op;

    /// <summary>POSTs <paramref name="body"/> to <paramref name="op"/> and reads the string field <paramref name="field"/>.</summary>
    private async Task<string> PostAsync(string op, byte[] body, string field, CancellationToken ct)
    {
        try
        {
            using var d = await _client.CallAsync(HttpMethod.Post, await AtAsync(op, ct).ConfigureAwait(false), body, ct).ConfigureAwait(false);
            return Json.Str(d.RootElement, field) ?? throw new RollSecretsException(0, "invalid", $"the answer has no {field}");
        }
        finally
        {
            CryptographicOperations.ZeroMemory(body); // it holds the input, as base64
        }
    }

    private static string B64(ReadOnlySpan<byte> data) => Convert.ToBase64String(data);

    private static byte[] Utf8(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        return Encoding.UTF8.GetBytes(text);
    }

    /// <summary>An empty context is no context, as the other clients send it.</summary>
    private static string? Context(string? context) => string.IsNullOrEmpty(context) ? null : context;

    /// <summary>
    /// <paramref name="data"/>'s ciphertext (<c>roll:v&lt;n&gt;:…</c>). <paramref name="context"/>, when
    /// given, must be given to decrypt it.
    /// </summary>
    public Task<string> EncryptAsync(ReadOnlyMemory<byte> data, string? context = null, CancellationToken ct = default) =>
        PostAsync("encrypt", Json.Write(("plaintext", B64(data.Span)), ("context", Context(context))), "ciphertext", ct);

    /// <summary><paramref name="text"/>'s ciphertext, the text sent as UTF-8.</summary>
    public async Task<string> EncryptAsync(string text, string? context = null, CancellationToken ct = default)
    {
        byte[] data = Utf8(text);
        try
        {
            return await EncryptAsync(data, context, ct).ConfigureAwait(false);
        }
        finally
        {
            CryptographicOperations.ZeroMemory(data);
        }
    }

    /// <summary>
    /// The plaintext of <paramref name="ciphertext"/> (audited, as a reveal), with the context it was
    /// encrypted with.
    /// </summary>
    /// <exception cref="RollSecretsException">400 when it doesn't open under this key and context, or its version is retired.</exception>
    public async Task<byte[]> DecryptAsync(string ciphertext, string? context = null, CancellationToken ct = default)
    {
        ArgumentNullException.ThrowIfNull(ciphertext);
        string plain = await PostAsync("decrypt", Json.Write(("ciphertext", ciphertext), ("context", Context(context))), "plaintext", ct).ConfigureAwait(false);
        try
        {
            return Convert.FromBase64String(plain);
        }
        catch (FormatException)
        {
            throw new RollSecretsException(0, "invalid", "the answer's plaintext isn't base64");
        }
    }

    /// <summary><see cref="DecryptAsync"/> of text that was encrypted as UTF-8.</summary>
    public async Task<string> DecryptStringAsync(string ciphertext, string? context = null, CancellationToken ct = default)
    {
        byte[] data = await DecryptAsync(ciphertext, context, ct).ConfigureAwait(false);
        try
        {
            return StrictUtf8.GetString(data);
        }
        catch (DecoderFallbackException)
        {
            throw new RollSecretsException(0, "invalid", "the plaintext isn't UTF-8 text: read it with DecryptAsync");
        }
        finally
        {
            CryptographicOperations.ZeroMemory(data);
        }
    }

    /// <summary>
    /// An Ed25519 signature over <paramref name="data"/> exactly as given (<c>roll:v&lt;n&gt;:…</c>):
    /// anyone with <see cref="PublicKeysAsync"/> checks it, with any Ed25519 library.
    /// </summary>
    public Task<string> SignAsync(ReadOnlyMemory<byte> data, CancellationToken ct = default) =>
        PostAsync("sign", Json.Write(("input", B64(data.Span))), "signature", ct);

    /// <summary><see cref="SignAsync(ReadOnlyMemory{byte}, CancellationToken)"/> of <paramref name="text"/> as UTF-8.</summary>
    public Task<string> SignAsync(string text, CancellationToken ct = default) => SignAsync(Utf8(text), ct);

    /// <summary>
    /// Whether <paramref name="signature"/> is the key's over <paramref name="data"/>. A signature by
    /// a retired or unknown version is false; a malformed one is a 400.
    /// </summary>
    public async Task<bool> VerifyAsync(ReadOnlyMemory<byte> data, string signature, CancellationToken ct = default)
    {
        ArgumentNullException.ThrowIfNull(signature);
        byte[] body = Json.Write(("input", B64(data.Span)), ("signature", signature));
        try
        {
            using var d = await _client.CallAsync(HttpMethod.Post, await AtAsync("verify", ct).ConfigureAwait(false), body, ct).ConfigureAwait(false);
            if (d.RootElement.ValueKind != JsonValueKind.Object || !d.RootElement.TryGetProperty("valid", out var v)
                || v.ValueKind is not (JsonValueKind.True or JsonValueKind.False))
            {
                throw new RollSecretsException(0, "invalid", "the answer has no valid");
            }
            return v.GetBoolean();
        }
        finally
        {
            CryptographicOperations.ZeroMemory(body);
        }
    }

    /// <summary><see cref="VerifyAsync(ReadOnlyMemory{byte}, string, CancellationToken)"/> of <paramref name="text"/> as UTF-8.</summary>
    public Task<bool> VerifyAsync(string text, string signature, CancellationToken ct = default) => VerifyAsync(Utf8(text), signature, ct);

    /// <summary>
    /// HMAC-SHA256 of <paramref name="data"/> (<c>roll:v&lt;n&gt;:…</c>), the same for the same input:
    /// a lookup column over encrypted fields (a blind index).
    /// </summary>
    public Task<string> HmacAsync(ReadOnlyMemory<byte> data, CancellationToken ct = default) =>
        PostAsync("hmac", Json.Write(("input", B64(data.Span))), "hmac", ct);

    /// <summary><see cref="HmacAsync(ReadOnlyMemory{byte}, CancellationToken)"/> of <paramref name="text"/> as UTF-8.</summary>
    public Task<string> HmacAsync(string text, CancellationToken ct = default) => HmacAsync(Utf8(text), ct);

    /// <summary>The key's Ed25519 public halves, every version, oldest first as roll lists them.</summary>
    public async Task<IReadOnlyList<TransitPublicKey>> PublicKeysAsync(CancellationToken ct = default)
    {
        using var d = await _client.CallAsync(HttpMethod.Get, await AtAsync("public", ct).ConfigureAwait(false), null, ct).ConfigureAwait(false);
        if (d.RootElement.ValueKind != JsonValueKind.Array)
        {
            throw new RollSecretsException(0, "invalid", "the answer isn't a list of public keys");
        }
        var keys = new List<TransitPublicKey>();
        foreach (var k in d.RootElement.EnumerateArray())
        {
            keys.Add(new TransitPublicKey(
                (int)Json.Num(k, "version"),
                Json.Str(k, "public_key") ?? "",
                Json.Str(k, "pem") ?? "",
                k.TryGetProperty("jwk", out var jwk) ? Json.StringMap(jwk) : new Dictionary<string, string>()));
        }
        return keys;
    }

    /// <summary>Names the key; a transit key holds nothing secret on this side.</summary>
    public override string ToString() => $"Transit {{ {Name} }}";
}

/// <summary>
/// One version of a transit key's Ed25519 public half: raw base64, PEM (SPKI) and a JWK (<c>kid</c>
/// <c>&lt;name&gt;:v&lt;version&gt;</c>). Nothing in it is secret.
/// </summary>
public sealed record TransitPublicKey(int Version, string PublicKey, string Pem, IReadOnlyDictionary<string, string> Jwk);
