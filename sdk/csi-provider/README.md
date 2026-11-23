# roll secrets' provider for the Secrets Store CSI Driver

A pod's volume gets a roll secrets environment's values as files, with no Kubernetes Secret
in etcd. Each pod logs in with its own service account token through a workload identity. A
login is reused for 5 minutes by mounts with the same token (rotation polls, a pod's other
volumes), so a scale-up stays within an identity's 120 logins a minute; a token roll refuses is
dropped and the pod logs in again. A login's last answers are kept in memory for as long, for
that login alone, so a rotation poll after a change reads only the keys that changed
(`…/values/delta`); every mount still asks roll, and reads strictly. With `dynamic: "true"`, each
mount reads in full.
See docs/secrets.md, "Kubernetes".
Standard library only: the driver's gRPC is served over Go 1.24's unencrypted HTTP/2, with the
protocol's seven messages encoded in `wire.go`.

1. Install the [Secrets Store CSI Driver](https://secrets-store-csi-driver.sigs.k8s.io) with
   the API's URL in its `tokenRequests`:
   `helm install csi-secrets-store secrets-store-csi-driver/secrets-store-csi-driver -n kube-system --set "tokenRequests[0].audience=https://api.roll.example"`.
2. Install the provider: `helm install roll-csi ./chart -n kube-system`, or
   `kubectl apply -f deploy/provider.yaml`.
3. Let a service account log in:
   `roll secrets identities add web --issuer <the cluster's issuer> --audience https://api.roll.example --subject system:serviceaccount:default:web --for-env production`.
   Add `--jwks <(kubectl get --raw /openid/v1/jwks)` when roll can't reach the issuer.
4. Mount a SecretProviderClass with `provider: roll` (parameters in docs/secrets.md).

Tests: `go test -race ./...` here (CI's `sdk` job), or without Go installed:
`docker run --rm -v "$PWD/..":/src -w /src/csi-provider golang:1.24-bookworm go test -race ./...`.
Built from `sdk/`: `docker build -f csi-provider/Dockerfile .`.
