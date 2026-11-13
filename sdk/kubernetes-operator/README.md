# The roll secrets Kubernetes operator

Keeps a Secret in step with a roll secrets environment, the way Infisical's and Doppler's
operators do. It pulls: it logs in with its own service account token through a workload
identity, so roll holds no credential for the cluster (the `kubernetes` sync pushes, with one).

```yaml
apiVersion: roll.sh/v1
kind: RollSecret
metadata:
  name: web          # the Secret's name too, unless spec.secretName says otherwise
  namespace: apps
spec:
  environment: production
  path: /api         # optional
```

- It checks each environment's digest every `ROLL_SYNC_EVERY` (default 30s) and reads values only
  when it moved, or the Secret is missing.
- It writes only Secrets it made: labelled `roll.sh/managed-by: rollsecret` and owned by their
  RollSecret, so deleting the RollSecret deletes them. A Secret of the same name someone else made
  is left alone, and the RollSecret's status says so.
- Values that don't resolve leave the Secret as it was, and the status names the key.

Install: `kubectl apply -f deploy/crd.yaml -f deploy/operator.yaml` after setting the API URL,
the identity and the token's audience in `deploy/operator.yaml`. The identity's issuer is the
cluster's service account issuer, its subject `system:serviceaccount:roll-system:roll-secrets-operator`.
roll fetches the issuer's keys over public HTTPS, so the cluster's issuer must be published (EKS,
GKE and AKS publish theirs); for a cluster whose issuer isn't, set `ROLL_TOKEN` to a roll token instead.

Tests: `go test ./...` (a fake Kubernetes API and a fake rolld).
