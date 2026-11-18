using System.Security.Cryptography;
using System.Text;
using System.Text.Json.Serialization;

namespace Roll.Secrets;

/// <summary>
/// A <c>roll-seal-v1</c> box, as crates/roll-secrets/src/crypto.rs makes it (see
/// <see cref="SealKey"/>): the version, the sender's ephemeral X25519 public key, the 12-byte nonce
/// and the AES-256-GCM ciphertext with its tag, each base64url without padding.
/// </summary>
public sealed record Sealed(
    [property: JsonPropertyName("v")] int V,
    [property: JsonPropertyName("epk")] string Epk,
    [property: JsonPropertyName("nonce")] string Nonce,
    [property: JsonPropertyName("ct")] string Ct)
{
    /// <summary>Names the version only.</summary>
    public override string ToString() => $"Sealed {{ V = {V} }}";
}

/// <summary>
/// The X25519 key a sealed resolve is sealed to. Keep it in the process: its public key
/// (<see cref="PublicKey"/>) is all the server needs.
/// </summary>
/// <remarks>
/// A <c>roll-seal-v1</c> box: X25519 between a fresh key pair and the recipient's key, HKDF-SHA256
/// with salt <c>epk ‖ recipient</c> and info <c>roll-seal-v1</c> into an AES-256-GCM key, a random
/// 12-byte nonce, and a context as the associated data, so a box opens only for the purpose it was
/// sealed for. X25519 is this library's own (RFC 7748); HKDF and AES-GCM are .NET's.
/// </remarks>
public sealed class SealKey : IDisposable
{
    private static readonly byte[] Info = Encoding.ASCII.GetBytes("roll-seal-v1");

    private readonly byte[] _private;
    private readonly byte[] _public;

    private SealKey(byte[] raw)
    {
        _private = raw;
        _public = X25519.PublicKey(raw);
    }

    /// <summary>A fresh key pair.</summary>
    public static SealKey Generate() => new(RandomNumberGenerator.GetBytes(32));

    /// <summary>A key from its 32 raw bytes (RFC 7748's encoding, as the other clients and the server use).</summary>
    public static SealKey FromPrivateBytes(ReadOnlySpan<byte> raw)
    {
        if (raw.Length != 32)
        {
            throw new ArgumentException("an X25519 private key is 32 bytes", nameof(raw));
        }
        return new SealKey(raw.ToArray());
    }

    /// <summary>The base64url public key, what a sealed resolve's <c>seal_to</c> takes.</summary>
    public string PublicKey => B64.Encode(_public);

    /// <summary>Opens a box sealed to this key for <paramref name="context"/>.</summary>
    /// <exception cref="CryptographicException">It doesn't open (another key, another context, changed).</exception>
    public byte[] Open(Sealed box, string context)
    {
        var fail = new CryptographicException("the box doesn't open");
        if (box is null || box.V != 1 || box.Epk is null || box.Nonce is null || box.Ct is null)
        {
            throw fail;
        }
        byte[] epk, nonce, ct;
        try
        {
            epk = B64.Decode(box.Epk);
            nonce = B64.Decode(box.Nonce);
            ct = B64.Decode(box.Ct);
        }
        catch (FormatException)
        {
            throw fail;
        }
        if (epk.Length != 32 || nonce.Length != 12 || ct.Length < 16)
        {
            throw fail;
        }
        byte[] shared = X25519.ScalarMult(_private, epk);
        byte[]? key = null;
        try
        {
            key = BoxKey(shared, epk, _public);
            if (key is null)
            {
                throw fail;
            }
            var plain = new byte[ct.Length - 16];
            using var gcm = new AesGcm(key, 16);
            gcm.Decrypt(nonce, ct.AsSpan(0, ct.Length - 16), ct.AsSpan(ct.Length - 16), plain, Encoding.UTF8.GetBytes(context ?? ""));
            return plain;
        }
        catch (CryptographicException)
        {
            throw fail;
        }
        finally
        {
            CryptographicOperations.ZeroMemory(shared);
            if (key is not null)
            {
                CryptographicOperations.ZeroMemory(key);
            }
        }
    }

    /// <summary>HKDF-SHA256 of the shared secret, salt epk ‖ recipient, info roll-seal-v1, 32 bytes; null for an all-zero secret (a small-order point).</summary>
    internal static byte[]? BoxKey(byte[] shared, byte[] epk, byte[] recipient)
    {
        int bits = 0;
        foreach (var b in shared)
        {
            bits |= b;
        }
        if (bits == 0)
        {
            return null;
        }
        var salt = new byte[64];
        epk.CopyTo(salt, 0);
        recipient.CopyTo(salt, 32);
        return HKDF.DeriveKey(HashAlgorithmName.SHA256, shared, 32, salt, Info);
    }

    /// <summary>Forgets the private key.</summary>
    public void Dispose() => CryptographicOperations.ZeroMemory(_private);

    /// <summary>Names the public key only.</summary>
    public override string ToString() => $"SealKey {{ {PublicKey} }}";
}

internal static class B64
{
    public static string Encode(ReadOnlySpan<byte> b) => Convert.ToBase64String(b).TrimEnd('=').Replace('+', '-').Replace('/', '_');

    public static byte[] Decode(string s)
    {
        if (s.Contains('+') || s.Contains('/') || s.Contains('='))
        {
            throw new FormatException("not base64url");
        }
        string t = s.Replace('-', '+').Replace('_', '/');
        return Convert.FromBase64String(t + new string('=', (4 - t.Length % 4) % 4));
    }
}
