using System.Security.Cryptography;
using System.Text;

namespace Roll.Secrets.Tests;

public class TransitTests
{
    private const string Token = "rlt_fake_token";
    private const string Plain = "4111-fake-card-0000";
    private const string Keys = "/v1/secrets/projects/prj_a/transit/";

    /// <summary>
    /// A transit key as roll answers it, in memory: ciphertexts and signatures are made-up ids that
    /// remember what they stand for, so a test can tell bytes went as base64 and came back the same.
    /// </summary>
    private static FakeRolld Rolld(List<string> bodies)
    {
        var sealedUnder = new Dictionary<string, (string? Context, string Input)>();
        var signed = new Dictionary<string, string>();
        return new FakeRolld(r =>
        {
            lock (bodies)
            {
                bodies.Add(r.Body);
            }
            if (r.Header("Authorization") != "Bearer " + Token)
            {
                return FakeRolld.Error(401, "unauthorized");
            }
            if (!r.Path.StartsWith(Keys))
            {
                return FakeRolld.Error(404, "not found");
            }
            var (key, op) = (r.Path[Keys.Length..].Split('/')[0], r.Path[Keys.Length..].Split('/')[1]);
            switch (key)
            {
                case "payroll":
                    return FakeRolld.Error(403, "this token can't write prj_a's secrets", "forbidden");
                case "broken" when op == "decrypt":
                    return new(200, new Dictionary<string, string> { ["plaintext"] = "fake plaintext, not base64!" });
                case not "cards":
                    return FakeRolld.Error(404, $"no key {key}");
            }
            var b = r.Method == "POST" ? r.Json() : default;
            string? Field(string name) => b.TryGetProperty(name, out var v) ? v.GetString() : null;
            lock (sealedUnder)
            {
                switch (op)
                {
                    case "encrypt":
                        var ct = "roll:v1:fakect" + sealedUnder.Count;
                        sealedUnder[ct] = (Field("context"), Field("plaintext")!);
                        return new(200, new Dictionary<string, string> { ["ciphertext"] = ct });
                    case "decrypt":
                        if (!sealedUnder.TryGetValue(Field("ciphertext")!, out var s) || s.Context != Field("context"))
                        {
                            return FakeRolld.Error(400, "the ciphertext doesn't open under this key and context");
                        }
                        return new(200, new Dictionary<string, string> { ["plaintext"] = s.Input });
                    case "sign":
                        var sig = "roll:v1:fakesig" + signed.Count;
                        signed[sig] = Field("input")!;
                        return new(200, new Dictionary<string, string> { ["signature"] = sig });
                    case "verify":
                        return new(200, new Dictionary<string, bool> { ["valid"] = signed.GetValueOrDefault(Field("signature")!) == Field("input") });
                    case "hmac":
                        var mac = Convert.ToHexString(SHA256.HashData(Convert.FromBase64String(Field("input")!)));
                        return new(200, new Dictionary<string, string> { ["hmac"] = "roll:v1:" + mac });
                    case "public":
                        return new(200, new[]
                        {
                            new Dictionary<string, object>
                            {
                                ["version"] = 1,
                                ["public_key"] = "ZmFrZS1wdWJsaWMta2V5",
                                ["pem"] = "-----BEGIN PUBLIC KEY-----\nZmFrZQ==\n-----END PUBLIC KEY-----\n",
                                ["jwk"] = new Dictionary<string, string> { ["kty"] = "OKP", ["crv"] = "Ed25519", ["x"] = "ZmFrZQ", ["kid"] = "cards:v1", ["alg"] = "EdDSA", ["use"] = "sig" },
                            },
                        });
                    default:
                        return FakeRolld.Error(404, "not found");
                }
            }
        });
    }

    private static RollSecretsClient Client(FakeRolld rolld) =>
        new(new RollSecretsOptions { Api = FakeRolld.Url, Token = Token, Project = "prj_a", HttpClient = rolld.Client() }, _ => null);

    /// <summary>
    /// Bytes and text go to roll as base64 and come back the same, with and without a context;
    /// signatures verify only over what was signed, and an HMAC is stable for one input.
    /// </summary>
    [Fact]
    public async Task TransitRoundTripsThroughRollBoundToItsContext()
    {
        var bodies = new List<string>();
        using var rolld = Rolld(bodies);
        using var c = Client(rolld);
        var cards = c.Transit("cards");

        byte[] raw = { 0, 255, 1, 0x80, 0xfe }; // not UTF-8: bytes stay bytes
        var ct = await cards.EncryptAsync(raw);
        Assert.Equal(raw, await cards.DecryptAsync(ct));
        Assert.DoesNotContain("context", bodies[0]); // none given, none sent

        var bound = await cards.EncryptAsync(Plain, "order:fake-1");
        Assert.Contains(Convert.ToBase64String(Encoding.UTF8.GetBytes(Plain)), bodies[^1]);
        Assert.DoesNotContain(Plain, bodies[^1]);
        Assert.Equal(Plain, await cards.DecryptStringAsync(bound, "order:fake-1"));

        var sig = await cards.SignAsync("fake webhook body");
        Assert.True(await cards.VerifyAsync("fake webhook body", sig));
        Assert.False(await cards.VerifyAsync("another fake body", sig));
        Assert.Equal(await cards.HmacAsync("fake@example.test"), await cards.HmacAsync(Encoding.UTF8.GetBytes("fake@example.test")));
        Assert.NotEqual(await cards.HmacAsync("fake@example.test"), await cards.HmacAsync("other@example.test"));

        var pub = Assert.Single(await cards.PublicKeysAsync());
        Assert.Equal(1, pub.Version);
        Assert.Equal("cards:v1", pub.Jwk["kid"]);
        Assert.StartsWith("-----BEGIN PUBLIC KEY-----", pub.Pem);
    }

    /// <summary>A wrong context, a refused or unknown key and a bad answer: errors with roll's status, quoting nothing sent.</summary>
    [Fact]
    public async Task TransitErrorsAndToStringHoldNoPlaintext()
    {
        using var rolld = Rolld(new List<string>());
        using var c = Client(rolld);
        var cards = c.Transit("cards");
        var bound = await cards.EncryptAsync(Plain, "order:fake-1");

        var failures = new List<RollSecretsException>
        {
            await Assert.ThrowsAsync<RollSecretsException>(() => cards.DecryptAsync(bound, "order:fake-2")),
            await Assert.ThrowsAsync<RollSecretsException>(() => cards.DecryptAsync(bound)),
        };
        Assert.All(failures, e => Assert.Equal(400, e.Status));

        var refused = await Assert.ThrowsAsync<RollSecretsException>(() => c.Transit("payroll").EncryptAsync(Plain));
        Assert.Equal((403, "forbidden"), (refused.Status, refused.Code));
        var unknown = await Assert.ThrowsAsync<RollSecretsException>(() => c.Transit("nope").SignAsync(Plain));
        Assert.Equal(404, unknown.Status);
        var garbled = await Assert.ThrowsAsync<RollSecretsException>(() => c.Transit("broken").DecryptStringAsync(bound));
        Assert.Equal("invalid", garbled.Code);
        failures.AddRange(new[] { refused, unknown, garbled });

        string b64 = Convert.ToBase64String(Encoding.UTF8.GetBytes(Plain));
        foreach (var e in failures)
        {
            foreach (var secret in new[] { Plain, b64, bound, Token, "fake plaintext" })
            {
                Assert.DoesNotContain(secret, e.ToString());
            }
        }
        Assert.Equal("Transit { cards }", cards.ToString());
        Assert.DoesNotContain(Token, c.ToString());
    }
}
