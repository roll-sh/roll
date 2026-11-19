# roll secrets for Spring Boot

Puts an environment's values in a Spring Boot app's `Environment`, as Spring Cloud Vault and
Spring Cloud AWS do: one import line, no code. It builds on the Java client (`sdk/java/secrets`,
`sh.roll:roll-secrets`) and needs Spring Boot 3 (built and tested against 3.5.6), which the app
brings: Maven coordinates `sh.roll:roll-secrets-spring`.

```properties
# application.properties
spring.config.import=roll-secrets:production/api
# Or, so a laptop without roll (or a token without a rule on the folder) still starts:
# spring.config.import=optional:roll-secrets:production/api
```

`roll-secrets:ENV` reads the environment's root, and `roll-secrets:ENV/FOLDER` one folder. Import
several, comma-separated, and a later one wins.

Settings (each also comes from the client's own environment variable when unset):

| Setting | Default |
| --- | --- |
| `roll.secrets.api-url` | `ROLL_API_URL` |
| `roll.secrets.token` | `ROLL_TOKEN` |
| `roll.secrets.project` | `ROLL_PROJECT`, else the token's own project |
| `roll.secrets.identity` | `ROLL_IDENTITY`: a workload identity's login with the workload's OIDC token instead of a token |
| `roll.secrets.oidc-token-file` | `ROLL_OIDC_TOKEN_FILE` (a Kubernetes projected token, read again for each login), else `ROLL_OIDC_TOKEN` or GitHub Actions |
| `roll.secrets.include-dynamic` | `false`: dynamic keys are left out and none is issued |
| `roll.secrets.refresh` | `false` |

Keys keep their names (`DB_URL`), and bind as environment variables do: `@Value("${db.url}")`,
`env.getProperty("db.url")` and `@ConfigurationProperties("db")` all find `DB_URL`.

**Failures.** A required import that can't be read (no answer, a 403, a key that doesn't resolve)
fails startup with a `RollSecretsConfigDataException` naming the import and roll's error. With
`optional:`, the app starts without it and logs one warning.

**Refreshing.** With `roll.secrets.refresh=true`, the client's watch follows the folder's digest (a
long poll: a change arrives within a second or two). When the digest moves, it swaps the new values
into the property source all at once and publishes a `RollSecretsRefreshedEvent` naming the keys
that changed. `Environment` lookups see the new values. Beans that copied a value at startup
(`@Value` fields, bound `@ConfigurationProperties`) keep the old one, so listen for the event and
rebuild what uses the value:

```java
@EventListener
void onSecrets(RollSecretsRefreshedEvent e) {
    if (e.keys().contains("DB_URL")) pool.reconnect(env.getProperty("DB_URL"));
}
```

Folder values that don't resolve are never swapped in: the app keeps the previous values, and a
warning is logged. The watch stops when the context closes. With `include-dynamic`, the watch also
renews the dynamic credentials' leases. Without `refresh`, nothing renews them, so a dynamic
credential lasts only its lease.

**No value in output.** A value never reaches a log line, an exception or a `toString()`: the
property source prints `RollSecretsPropertySource[roll-secrets:production/api, 3 keys]`.

Without Boot's config import (plain Spring), add the source yourself:

```java
RollSecrets client = RollSecrets.builder().build();
env.getPropertySources().addFirst(RollSecretsPropertySource.load(client, "production", "/api", false));
```

Tests: a fake rolld (`com.sun.net.httpserver`) and real Spring Boot contexts. Install the client
first, then run `mvn -B -q test` here (CI's `java` job does both). Without Java installed:

```
docker run --rm -v "$PWD/..":/src -w /src/secrets maven:3.9-eclipse-temurin-17 mvn -B -q install -DskipTests
docker run --rm -v "$PWD/..":/src -w /src/secrets-spring maven:3.9-eclipse-temurin-17 mvn -B -q test
```

(Give both a shared `-v ~/.m2:/root/.m2` so the second finds what the first installed.)
