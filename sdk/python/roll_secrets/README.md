# roll secrets for Python

A client for roll secrets (docs/secrets.md, "Consumption") with the same surface as
`@roll/secrets` and `sdk/go/secrets`: an environment's values, the runtime resolve (sealed to a
key only this process holds, if asked), the digest that moves when the values would, and a watch
that follows it and renews dynamic leases. The standard library, but for sealed resolves, which
need `cryptography` (`pip install "roll-secrets[seal]"`).

```python
import roll_secrets

# ROLL_API_URL and ROLL_TOKEN (a roll token with a rule on the project), or ROLL_IDENTITY for a
# workload identity's login with the workload's own OIDC token: no long-lived token in CI.
c = roll_secrets.from_env()
v = c.values("production")                       # v.values["DATABASE_URL"]

key = roll_secrets.generate_key()               # sealed to a key only this process holds
v = c.resolve_sealed("production", "app:web", key)

c.watch("production", lambda v: reload(v.values))   # until `stop` (a threading.Event) is set
```

Tests: `PYTHONPATH=src python -m unittest discover -s tests` (CI's `sdk` job).
