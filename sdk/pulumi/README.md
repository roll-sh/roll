# roll secrets from Pulumi

Pulumi uses roll's Terraform provider (`sdk/terraform-provider-roll`) through Pulumi's
[`terraform-provider` package](https://www.pulumi.com/registry/packages/terraform-provider/), which
bridges any Terraform provider. It runs a local build: roll needs no release in a registry.
docs/secrets.md, "Pulumi", has what's in it and what's left out.

## Install

In a Pulumi project, build the provider into `./bin` and add it:

```sh
(cd path/to/roll/sdk/terraform-provider-roll && go build -o "$OLDPWD/bin/terraform-provider-roll" .)
pulumi package add terraform-provider ./bin/terraform-provider-roll
```

`package add` writes the package into `Pulumi.yaml` (the path as given) and generates the SDK in
`sdks/roll`: `@pulumi/roll` for TypeScript, `pulumi_roll` for Python, `github.com/pulumi/pulumi-terraform-provider/sdks/go/roll`
for Go. Commit `Pulumi.yaml`, not `bin/` or `sdks/`: every machine that runs the program (CI too)
builds the binary at the same path, and `pulumi install` generates the SDK again. Pulumi downloads
its `terraform-provider` plugin the first time.

`sdk.sh [OUT]` does the same outside a project: it builds the provider into `OUT/bin` and generates
the SDKs into `OUT/sdk/{nodejs,python,go}` (`ROLL_PULUMI_LANGUAGES` picks others). Those SDKs name
the binary by its absolute path. Both scripts build with `go`, or with Docker's `golang` image
when Go isn't installed (`ROLL_GO_CACHE` keeps its caches); `ROLL_TF_PROVIDER` takes a binary
already built instead.

## Configure

```sh
pulumi config set roll:apiUrl https://api.roll.example
pulumi config set roll:project prj_…
pulumi config set --secret roll:token rlt_…    # or ROLL_API_URL, ROLL_PROJECT and ROLL_TOKEN
```

`example/` is a program: an environment, a key written write-only, a token for CI and the
environment's values.

## Test

`e2e.sh` copies `example/` to a temporary directory and runs `pulumi up` and `pulumi destroy`
against the Terraform provider's fake rolld (its test `TestServeFakeRolld`) on a local backend.
It checks that:

- roll got the value;
- neither the value nor the token is in the state in the clear;
- the key's outputs hold no value;
- destroy removed the environment and the key, and revoked the token.

It needs `pulumi`, `node` and `npm`, and `go` or Docker.
