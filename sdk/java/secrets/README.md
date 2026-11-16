# roll secrets for Java

A client for roll secrets (docs/secrets.md, "Consumption"), with the same surface as
`@roll/secrets` and `sdk/go/secrets`: an environment's values, the runtime resolve (sealed to a
key only this process holds, if asked), the digest that moves when the values would, and a watch
that follows it and renews dynamic leases. Java 17 and the JDK only (`java.net.http`,
`javax.crypto`); Maven coordinates `sh.roll:roll-secrets`.

```java
import sh.roll.secrets.*;

// ROLL_API_URL and ROLL_TOKEN (a roll token with a rule on the project), or ROLL_IDENTITY for a
// workload identity's login with the workload's own OIDC token (GitHub Actions, ROLL_OIDC_TOKEN
// or ROLL_OIDC_TOKEN_FILE): no long-lived token in CI. ROLL_PROJECT when the token doesn't say.
RollSecrets c = RollSecrets.builder().build();
// Or explicitly: RollSecrets.builder().api(url).token(token).project("prj_…").build()

Values v = c.values("production", "");          // v.get("DATABASE_URL"); v.problems()
String one = c.get("production", "/billing", "STRIPE_KEY");

// What a running service gets, sealed to a key only this process holds.
SealKey key = SealKey.generate();
v = c.resolveSealed("production", "", "app:web", key);

// Reload when the environment changes (a long poll: within a second or two); dynamic
// credentials' leases are renewed meanwhile. close() stops it.
Watch w = c.watch("production", WatchOptions.of(vals -> reload(vals.values())).onError(e -> log.warn(e.getMessage())));

// Read per request? Keep answers 30 s, then revalidate (a 304 while unchanged).
RollSecrets cached = RollSecrets.builder().cacheTtl(Duration.ofSeconds(30)).build();
```

Errors are `RollSecretsException` (`status()`, `code()`: `forbidden`, `unresolved`…); neither
they nor `Values.toString()` hold a value.

`digest(env, path)` reads the digest; `digestSince(env, path, since, wait)` long-polls it
(`…/digest?since=&wait=`, at most 25 s) and answers as soon as it moves. A server without long
polls answers at once, and the watch then asks again every `WatchOptions.every` (5 s).

Tests: `mvn -B -q test` here (CI's `sdk` job), or without Java installed:
`docker run --rm -v "$PWD":/src -w /src maven:3.9-eclipse-temurin-17 mvn -B -q test`.
