# roll secrets for Python

A client for roll secrets (docs/secrets.md, "Consumption") with the same surface as
`@roll/secrets` and `sdk/go/secrets`: an environment's values, the runtime resolve (sealed to a
key only this process holds, if asked), the digest that moves when the values would, a watch that
follows it and renews dynamic leases, and one key's writes. The standard library, but for sealed
resolves, which need `cryptography` (`pip install "roll-secrets[seal]"`).

```python
import roll_secrets

# ROLL_API_URL and ROLL_TOKEN (a roll token with a rule on the project), or ROLL_IDENTITY for a
# workload identity's login with the workload's own OIDC token: no long-lived token in CI.
c = roll_secrets.from_env()
v = c.values("production")                       # v.values["DATABASE_URL"]

key = roll_secrets.generate_key()               # sealed to a key only this process holds
v = c.resolve_sealed("production", "app:web", key)

c.watch("production", lambda v: reload(v.values))   # until `stop` (a threading.Event) is set

# Read per request? Keep answers 30 s, then revalidate (a 304 while unchanged).
c = roll_secrets.Client(api, token, cache_ttl=30)

# Nothing here renews a lease? Issue no dynamic credential: those keys are left out and named in
# v.dynamic, and the answer can be kept by cache_ttl.
v = c.values("production", dynamic=False)

# One key's writes. set replaces a link the key had; delete of a key that isn't there answers
# {"deleted": False, ...}. A protected environment, or a folder that needs approval, raises
# SecretsError with the code "approval_required" (open a change request instead). Errors never
# hold the value, and both drop the cached reads.
w = c.set("staging", "FEATURE_FLAGS", "beta", path="/api", comment="rollout")   # w["secret"]["version"]
d = c.delete("staging", "OLD_KEY", path="/api")                                 # d["deleted"]
```

Tests: `PYTHONPATH=src python -m unittest discover -s tests` (CI's `sdk` job).
