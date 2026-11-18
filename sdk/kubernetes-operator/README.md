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

- It long-polls each RollSecret's digest, so a change reaches its Secret within seconds, and checks
  every `ROLL_SYNC_EVERY` anyway (default 30s). It reads values only when the digest moved, or the
  Secret is missing. Past 32 held polls per login, roll answers at once, and those RollSecrets fall
  back to the interval.
- It writes only Secrets it made: labelled `roll.sh/managed-by: rollsecret` and owned by their
  RollSecret, so deleting the RollSecret deletes them. A Secret of the same name someone else made
  is left alone, and the RollSecret's status says so.
- Values that don't resolve leave the Secret as it was, and the status names the key.
- **What reads it restarts when it changes**, as Infisical's reload annotation and Vault Secrets
  Operator's `rolloutRestartTargets` do: `spec.rolloutRestart: [{kind: Deployment, name: web}]`
  (Deployments, StatefulSets and DaemonSets of the RollSecret's namespace) get a new
  `roll.sh/restartedAt` on their pod template, as `kubectl rollout restart` does: only when the
  Secret's data changed, never when it's first made. A workload opts in, as Reloader's do: its
  `roll.sh/restart-on` annotation names the RollSecrets (comma-separated) that may restart it, so
  creating a RollSecret alone restarts nothing. Every target is tried; one that couldn't be
  restarted (gone, not opted in) is named in `status.pendingRestarts` (why: `status.restartError`),
  and tried again on each pass until it is; the Secret itself stays on its cheap path meanwhile.
- **Each namespace logs in as itself.** Outside the operator's own namespaces
  (`roll.namespaces`, default the release's), a RollSecret names a service account of its own
  namespace and an identity for it:
  ```yaml
  spec:
    environment: production
    serviceAccountName: app      # the operator requests a 10-minute token for apps/app
    identity: idn_…              # subject system:serviceaccount:apps:app
  ```
  So whoever may create a RollSecret in a namespace reaches only what that namespace's
  identities allow, never the operator's own identity or another team's (as Vault Secrets
  Operator's per-namespace auth and External Secrets' namespaced stores do).

Install with Helm:

```
helm install roll-secrets sdk/kubernetes-operator/chart -n roll-system --create-namespace \
  --set roll.apiUrl=https://api.roll.example --set roll.identity=idn_…
```

(`roll.tokenSecret.name` instead of `roll.identity` takes a roll token from an existing Secret.)
Or plainly: `kubectl apply -f deploy/crd.yaml -f deploy/operator.yaml` after setting the API URL,
the identity and the token's audience in `deploy/operator.yaml`. The identity's issuer is the
cluster's service account issuer, its subject `system:serviceaccount:roll-system:roll-secrets-operator`.
roll fetches the issuer's keys over public HTTPS, so the cluster's issuer must be published (EKS,
GKE and AKS publish theirs); for a cluster whose issuer isn't, set `ROLL_TOKEN` to a roll token instead.

Outside a cluster: `KUBE_API`, `KUBE_TOKEN` and `KUBE_CA_FILE` (the cluster's CA).

Tests: `go test ./...` (a fake Kubernetes API and a fake rolld).
