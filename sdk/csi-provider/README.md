# roll secrets' provider for the Secrets Store CSI Driver

A pod's volume gets a roll secrets environment's values as files, with no Kubernetes Secret
in etcd. Each pod logs in with its own service account token through a workload identity.
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
