# roll secrets for .NET

A client for roll secrets (docs/secrets.md, "Consumption"), with the same surface as
`@roll/secrets`, `sdk/go/secrets` and `sdk/java/secrets`: an environment's values, the runtime
resolve (sealed to a key only this process holds, if asked), the digest that moves when the values
would, a watch that follows it and renews dynamic leases, and encryption keys (transit). .NET 8 and the base class library
only (`System.Net.Http`, `System.Text.Json`, `System.Security.Cryptography`; X25519, which .NET
lacks, is the library's own, checked against RFC 7748). NuGet package `Roll.Secrets`.

```csharp
using Roll.Secrets;

// ROLL_API_URL and ROLL_TOKEN (a roll token with a rule on the project), or ROLL_IDENTITY for a
// workload identity's login with the workload's own OIDC token (GitHub Actions, ROLL_OIDC_TOKEN
// or ROLL_OIDC_TOKEN_FILE): no long-lived token in CI. ROLL_PROJECT when the token doesn't say.
using var c = new RollSecretsClient();
// Or explicitly: new RollSecretsClient(new RollSecretsOptions { Api = url, Token = token, Project = "prj_…" })

var v = await c.ValuesAsync("production");          // v["DATABASE_URL"]; v.Problems
var one = await c.GetAsync("production", "/billing", "STRIPE_KEY");

// What a running service gets, sealed to a key only this process holds.
using var key = SealKey.Generate();
v = await c.ResolveSealedAsync("production", "", "app:web", key);

// Reload when the environment changes (a long poll: within a second or two); dynamic
// credentials' leases are renewed meanwhile. Disposing it stops it.
await using var w = c.Watch("production", new WatchOptions
{
    OnChange = vals => Reload(vals.Values),
    OnError = e => log.LogWarning("{Message}", e.Message),
});

// Read per request? Keep answers 30 s, then ask for what changed since (`…/values/delta`:
// nothing while unchanged, only the changed keys after a change). A watch reads changes the same way.
using var cached = new RollSecretsClient(new RollSecretsOptions { CacheTtl = TimeSpan.FromSeconds(30) });

// An encryption key (docs/secrets.md, "Encryption keys"): the key stays in roll.
var cards = c.Transit("cards");
var ct = await cards.EncryptAsync(cardNumber, $"order:{id}");   // roll:v1:…, bound to the context
var number = await cards.DecryptStringAsync(ct, $"order:{id}"); // audited, as a reveal
var index = await cards.HmacAsync(email);                       // a lookup column over encrypted emails
```

`Transit(name)` takes bytes (`ReadOnlyMemory<byte>`) or text (sent as UTF-8) for
`EncryptAsync(data, context)`, `SignAsync(data)`, `VerifyAsync(data, signature)` and
`HmacAsync(data)`; `DecryptAsync(ciphertext, context)` answers bytes and `DecryptStringAsync` text.
Ciphertexts, signatures and HMACs name the key version that made them (`roll:v<n>:…`), so they keep
working after a rotation. Signatures are Ed25519 over the bytes as given: `PublicKeysAsync()` is
every version's public key (`Version`, `PublicKey` as base64, `Pem`, `Jwk`), for a service that
checks them without roll. A wrong context or a retired version is a 400, a key the token may not
use a 403. Making, rotating and retiring keys is `roll secrets transit`'s, not the client's.

Errors are `RollSecretsException` (`Status`, `Code`: `forbidden`, `unresolved`…); neither they
nor `SecretValues.ToString()` hold a value (nor, for transit, a plaintext, ciphertext or signed input), and `RollSecretsOptions` is a class whose `ToString()`
doesn't print the token.

`DigestAsync(env, path)` reads the digest; `DigestSinceAsync(env, path, since, wait)` long-polls
it (`…/digest?since=&wait=`, at most 25 s) and answers as soon as it moves. A server without long
polls answers at once, and the watch then asks again every `WatchOptions.Every` (5 s).
`RenewAsync(lease)` keeps a dynamic credential.

## `IConfiguration`

`Roll.Secrets.Configuration` (it needs `Microsoft.Extensions.Configuration`, so it's a package of
its own) adds an environment as a configuration source; `__` in a key is the section separator, as
with environment variables (`DB__HOST` is `DB:HOST`):

```csharp
using Roll.Secrets.Configuration;

builder.Configuration.AddRollSecrets("production", s =>
{
    s.Path = "/web";          // a folder (default the root)
    s.ReloadOnChange = true;  // a watch: IOptionsMonitor sees changes
});
```

`s.Consumer` resolves as a consumer instead, `s.Client` takes `RollSecretsOptions` (default: the
environment), and `s.Optional` keeps a failed first load from stopping the app.

Tests: `dotnet test` here (CI's `dotnet` job), or without .NET installed, `./dotnet-in-docker.sh test`
(the `mcr.microsoft.com/dotnet/sdk:8.0` image, NuGet's cache in `~/.cache/roll-secrets-nuget`;
`ROLL_DOTNET_NAME` names the container).
