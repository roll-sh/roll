using System.Security.Cryptography;
using System.Text;

namespace Roll.Secrets.Tests;

public class SealTests
{
    // A box sealed by @roll/secrets' sealTo (the same box as the server's seal_to), with a fixed
    // key: the same vector as sdk/go/secrets, sdk/python/roll_secrets and sdk/java/secrets.
    internal const string VectorKey = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA";
    internal const string VectorContext = "roll-secrets:prj_a:production:app:web";
    internal static readonly Sealed VectorBox = new(1, "aAGjVaESpknkSH_uG48K1a_unnp97Gh4uRwOYWrjHkk", "yct4_uAUdnUKxskR",
        "-UbR7dzvRmoXgBHnbyoW00qyGY07tIB_0vx6VrXQhPPRNns7PU3irIJq5M5Xjto");

    internal static SealKey Vector() => SealKey.FromPrivateBytes(B64.Decode(VectorKey));

    [Fact]
    public void ABoxSealedByRollOpensOnlyForItsContext()
    {
        using var k = Vector();
        Assert.Equal("{\"DB_URL\":\"postgres://db/shop\"}", Encoding.UTF8.GetString(k.Open(VectorBox, VectorContext)));
        Assert.Throws<CryptographicException>(() => k.Open(VectorBox, "roll-secrets:prj_a:staging:app:web"));
        Assert.Throws<CryptographicException>(() => k.Open(VectorBox with { Ct = VectorBox.Ct[..^2] + "AA" }, VectorContext));
        using var other = SealKey.Generate();
        Assert.Throws<CryptographicException>(() => other.Open(VectorBox, VectorContext));
        // The public key seal_to sends, as Node's own X25519 derives it from the vector's key.
        Assert.Equal("B6N8vBQgk8i3VdwbEOhstCY3StFqqFPtC9_AsrhtHHw", k.PublicKey);
        Assert.DoesNotContain(VectorKey, k.ToString());
    }

    // RFC 7748, section 5.2: two single multiplications, then the function iterated on its output.
    [Theory]
    [InlineData("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4", "e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c", "c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552")]
    [InlineData("4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d", "e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493", "95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957")]
    public void X25519MatchesRfc7748(string scalar, string u, string output)
    {
        Assert.Equal(output, Convert.ToHexString(X25519.ScalarMult(Convert.FromHexString(scalar), Convert.FromHexString(u))).ToLowerInvariant());
    }

    [Fact]
    public void X25519IteratedMatchesRfc7748()
    {
        byte[] k = Convert.FromHexString("0900000000000000000000000000000000000000000000000000000000000000"), u = (byte[])k.Clone();
        for (int i = 1; i <= 1000; i++)
        {
            var r = X25519.ScalarMult(k, u);
            u = k;
            k = r;
            if (i == 1)
            {
                Assert.Equal("422c8e7a6227d7bca1350b3e2bb7279f7897b87bb6854b783c60e80311ae3079", Convert.ToHexString(k).ToLowerInvariant());
            }
        }
        Assert.Equal("684cf59ba83309552800ef566f2f4d3c1c3887c49360e3875f2eb94d99532c51", Convert.ToHexString(k).ToLowerInvariant());
    }

    // RFC 7748, section 6.1: Alice's and Bob's public keys and the secret they share.
    [Fact]
    public void X25519DiffieHellmanMatchesRfc7748()
    {
        var a = Convert.FromHexString("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        var b = Convert.FromHexString("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
        var aPub = X25519.PublicKey(a);
        var bPub = X25519.PublicKey(b);
        Assert.Equal("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a", Convert.ToHexString(aPub).ToLowerInvariant());
        Assert.Equal("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f", Convert.ToHexString(bPub).ToLowerInvariant());
        const string shared = "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742";
        Assert.Equal(shared, Convert.ToHexString(X25519.ScalarMult(a, bPub)).ToLowerInvariant());
        Assert.Equal(shared, Convert.ToHexString(X25519.ScalarMult(b, aPub)).ToLowerInvariant());
    }

    [Fact]
    public void ABoxFromASmallOrderKeyDoesNotOpen()
    {
        // u = 0 is a point of small order: the shared secret is all zeros, which must be refused.
        using var k = Vector();
        var box = VectorBox with { Epk = B64.Encode(new byte[32]) };
        Assert.Throws<CryptographicException>(() => k.Open(box, VectorContext));
    }
}
