# roll secrets on each platform

This guide gets an environment's values into the place your code runs: CI, Kubernetes, a VM, a
Windows server, AWS Lambda, or your own machine.

On every platform that has an identity of its own, use a **workload identity**. The job, pod or
machine logs in with its own short-lived token, so no roll token sits in its settings, and a leaked
log can't reuse a login after a few minutes. Make the identity once, with the narrowest scope that
works:

```sh
roll secrets identities add <name> --for-env <environment> …
```

The rules are in the reference: ["Workload identities (OIDC)"](../secrets.md#workload-identities-oidc)
and ["AWS IAM"](../secrets.md#aws-iam). Where a platform has no identity, use a roll token with one
rule ([Getting started](getting-started.md), step 8).

The examples use `https://roll.example.com` for your roll's API, `shop` for the project and `idn_…`
for the identity's id, which `identities add` prints.

- [CI](#ci): [GitHub Actions](#github-actions), [GitLab CI](#gitlab-ci), [Azure
  Pipelines](#azure-pipelines)
- [Kubernetes](#kubernetes): [the operator](#the-operator), [the CSI provider](#the-csi-provider),
  [External Secrets Operator](#external-secrets-operator), [a sidecar](#a-sidecar)
- [A VM or a server](#a-vm-or-a-server): [AWS EC2](#aws-ec2), [Google Compute
  Engine](#google-compute-engine), [Azure VMs](#azure-vms), [bare metal](#bare-metal-and-on-premises)
- [Windows](#windows)
- [AWS Lambda](#aws-lambda)
- [Local development](#local-development)

## CI

### GitHub Actions

1. Make an identity for the repository:

   ```sh
   roll secrets identities add ci --github acme/web --claim ref=refs/heads/* --for-env staging
   ```

   For production, narrow it to one branch: `--claim ref=refs/heads/main --for-env production`.

2. Put the environment into the job with roll's action:

   ```yaml
   permissions:
     id-token: write
   steps:
     - uses: roll-sh/roll/sdk/github-action@main
       with:
         api-url: https://roll.example.com
         identity: idn_…
         environment: staging
     - run: make test            # DATABASE_URL and the rest are in the environment
   ```

   The action masks every value before it prints anything. Its other inputs (`token`, `export`,
   `client-id`, `scan`) are in ["GitHub Actions"](../secrets.md#github-actions).

Or use the CLI on the runner. With `ROLL_IDENTITY` set, `roll` logs in with the job's own token.
This job checks the environment and the repository before it runs the tests:

```yaml
jobs:
  test:
    runs-on: ubuntu-latest
    permissions: { id-token: write, contents: read }
    env: { ROLL_API_URL: https://roll.example.com, ROLL_IDENTITY: idn_…, ROLL_ENV: staging }
    steps:
      - uses: actions/checkout@v4
      # roll on the runner's PATH; then:
      - run: roll secrets check .env.example   # every key the code reads is set in staging
      - run: roll secrets scan .               # no stored value was committed
      - run: roll secrets health               # nothing expired, missing, exposed or in the browser
      - run: roll run -- npm test              # the tests get staging's values, never on disk
```

Each step sees only staging, because check, scan and health answer only what the identity can
read.

### GitLab CI

GitLab signs a job's token as `https://gitlab.com` (or your instance's URL), with the project and
the ref in its subject. GitLab.com signs for every project it hosts, so the subject must name your
project exactly.

1. Make an identity for the project's `main` branch:

   ```sh
   roll secrets identities add deploy --issuer https://gitlab.com \
     --subject project_path:acme/web:ref_type:branch:ref:main --for-env production
   ```

   The audience is your roll's API URL by default.

2. Ask GitLab for a token with that audience, in the variable `roll` reads:

   ```yaml
   deploy:
     id_tokens:
       ROLL_OIDC_TOKEN:
         aud: https://roll.example.com
     variables:
       ROLL_API_URL: https://roll.example.com
       ROLL_IDENTITY: idn_…
     script:
       - roll run -e production -- ./deploy.sh
   ```

To match GitLab's other claims (`/groups_direct`, `environment`), add `--claim`. See ["Claims"](../secrets.md#workload-identities-oidc).

To push values into GitLab's CI/CD variables instead, use the [`gitlab` sync](../secrets.md#gitlab).

### Azure Pipelines

Azure DevOps signs a pipeline's token as its organization, for Azure's own audience, with the
pipeline as its subject.

1. Make an identity for one pipeline. The organization's id is the GUID in
   `$(System.CollectionId)`:

   ```sh
   roll secrets identities add deploy --for-env production --audience api://AzureADTokenExchange \
     --issuer https://vstoken.dev.azure.com/<organization id> --subject p://acme/shop/deploy
   ```

   The dashboard's **New identity** has an Azure Pipelines choice that fills in the issuer and the
   audience.

2. Map the job's access token into the step, and `roll` asks Azure for the token itself:

   ```yaml
   variables:
     ROLL_API_URL: https://roll.example.com
     ROLL_IDENTITY: idn_…
   steps:
     - script: roll run --env production -- ./deploy.sh
       env:
         SYSTEM_ACCESSTOKEN: $(System.AccessToken)   # a secret: a script sees it only when mapped
   ```

For production, log in through a service connection instead, so its approvals and checks guard
the login. See ["Azure Pipelines"](../secrets.md#azure-pipelines).

### Other CI systems

- If the system gives the job an OIDC token, make an identity for its issuer and subject, and put
  the token in `ROLL_OIDC_TOKEN` (or a file named by `ROLL_OIDC_TOKEN_FILE`).
- Otherwise, use a roll token in the system's secret variables, as `ROLL_TOKEN`.
- `roll run` masks values in the job's log. On **Jenkins**, also set `ROLL_MASK=1` in the job's
  environment, or stdout reaches the log unmasked. See ["`roll secrets` and `roll run`"](../secrets.md#roll-secrets-and-roll-run).

## Kubernetes

Pick the way that fits what the pod needs:

| You want | Use |
| --- | --- |
| a Kubernetes Secret that follows the environment | [the operator](#the-operator) |
| files in the pod, and no Secret in etcd | [the CSI provider](#the-csi-provider) |
| to keep the External Secrets Operator you already run | [External Secrets Operator](#external-secrets-operator) |
| values over HTTP inside the pod, as on a VM | [a sidecar](#a-sidecar) |

The operator, the CSI provider and the sidecar log in with a service account's own token. roll
fetches the cluster's keys from its service account issuer. EKS, GKE and AKS publish theirs. For a
cluster whose issuer roll can't reach, pin its keys on the identity: `--jwks <(kubectl get --raw
/openid/v1/jwks)`.

The rules for all of them are in ["Kubernetes"](../secrets.md#kubernetes).

### The operator

1. Make an identity for the operator's service account:

   ```sh
   roll secrets identities add k8s-operator --issuer <the cluster's issuer> \
     --subject system:serviceaccount:roll-system:roll-secrets-operator --for-env production
   ```

2. Install the operator:

   ```sh
   helm install roll-secrets sdk/kubernetes-operator/chart -n roll-system --create-namespace \
     --set roll.apiUrl=https://roll.example.com --set roll.identity=idn_…
   ```

3. Add a `RollSecret`. The operator keeps a Secret of the same name in step with the folder:

   ```yaml
   apiVersion: roll.sh/v1
   kind: RollSecret
   metadata:
     name: web
     namespace: apps
   spec:
     environment: production
     path: /api
   ```

Outside the operator's own namespace, a `RollSecret` logs in as a service account of its own
namespace, and `spec.rolloutRestart` restarts what reads the Secret. See the operator's
[README](../../sdk/kubernetes-operator/README.md).

### The CSI provider

1. Install the [Secrets Store CSI Driver](https://secrets-store-csi-driver.sigs.k8s.io) with your
   roll's API URL as its token audience:

   ```sh
   helm install csi-secrets-store secrets-store-csi-driver/secrets-store-csi-driver -n kube-system \
     --set "tokenRequests[0].audience=https://roll.example.com"
   ```

2. Install the provider:

   ```sh
   helm install roll-csi sdk/csi-provider/chart -n kube-system
   ```

3. Make an identity for the pod's service account:

   ```sh
   roll secrets identities add web --issuer <the cluster's issuer> --audience https://roll.example.com \
     --subject system:serviceaccount:default:web --for-env production
   ```

4. Add a `SecretProviderClass` with `provider: roll`, and mount it in the pod. Its parameters are
   in ["Kubernetes"](../secrets.md#kubernetes).

### External Secrets Operator

One command makes a token for the environment and folder, and prints the `Secret`, the
`SecretStore` and an `ExternalSecret`:

```sh
roll secrets -e production --path /api kubernetes --namespace shop | kubectl apply -f -
```

The dashboard's **Export** menu has the same, as "External Secrets Operator…". The cluster calls
roll, and roll never calls the cluster, so this works for a cluster with a private API server. See
["Pulling instead: External Secrets Operator"](../secrets.md#pulling-instead-external-secrets-operator).

### A sidecar

The local agent runs beside the app in the pod, as a native sidecar. It logs in as the pod's
service account, with a projected token for the audience `roll`:

```sh
roll secrets identities add web --issuer <the cluster's issuer> --audience roll \
  --subject system:serviceaccount:shop:web --for-env production
```

The agent's token file sits in a volume that only the pod's two containers share, and both
containers run as one user so the app can read it.

```yaml
spec:
  serviceAccountName: web
  securityContext: { runAsUser: 10001, runAsGroup: 10001 }
  volumes:
    - name: roll-oidc
      projected:
        sources:
          - serviceAccountToken: { audience: roll, expirationSeconds: 3600, path: token }
    - name: roll-agent
      emptyDir: { medium: Memory }
  initContainers:
    - name: roll-secrets-agent
      image: ghcr.io/roll-sh/roll:latest       # any image with the roll binary
      restartPolicy: Always                    # a native sidecar: up before the app starts
      args: [secrets, -p, shop, -e, production, agent, --token-file, /run/roll/agent-token, --prefetch, "production:/"]
      env:
        - { name: ROLL_API_URL, value: https://roll.example.com }
        - { name: ROLL_IDENTITY, value: idn_… }
        - { name: ROLL_OIDC_TOKEN_FILE, value: /var/run/secrets/roll/token }
      volumeMounts:
        - { name: roll-oidc, mountPath: /var/run/secrets/roll, readOnly: true }
        - { name: roll-agent, mountPath: /run/roll }
  containers:
    - name: web
      image: shop/web
      volumeMounts:
        - { name: roll-agent, mountPath: /run/roll, readOnly: true }
```

The kubelet's `httpGet` probes come from the node, which can't reach the pod's `127.0.0.1`. To
probe the agent, use an `exec` probe. The app reads values as on a VM (next section).

## A VM or a server

The local agent, `roll secrets agent`, answers the app's requests for values on `127.0.0.1:2773`,
from a cache that follows changes in roll. It's part of the `roll` binary. Its rules are in ["A
local agent"](../secrets.md#a-local-agent).

1. Install `roll` ([Getting started](getting-started.md), step 1).
2. Set the machine up to log in as itself: [AWS EC2](#aws-ec2), [Google Compute
   Engine](#google-compute-engine), [Azure VMs](#azure-vms) or [bare
   metal](#bare-metal-and-on-premises).
3. Run the agent as a systemd unit, as the app's own user (the agent's token file is 0600):

   ```ini
   [Unit]
   Description=roll secrets agent
   After=network-online.target

   [Service]
   Environment=ROLL_API_URL=https://roll.example.com ROLL_IDENTITY=idn_…
   ExecStart=/usr/local/bin/roll secrets -p shop -e production agent --token-file /run/roll/agent-token --prefetch production:/
   User=shop
   RuntimeDirectory=roll
   Restart=always

   [Install]
   WantedBy=multi-user.target
   ```

4. Read a value from the app, with the request token from the file:

   ```sh
   curl -sH "X-Roll-Token: $(cat /run/roll/agent-token)" http://127.0.0.1:2773/secrets/DATABASE_URL
   ```

   `GET /secrets?env=production&path=/` answers a whole folder as JSON.

To start a single process with its values instead of running the agent, use `roll run` in the
process's unit. It logs in the same way.

### AWS EC2

Make an identity for the instance profile's role. Nothing else is needed on the instance: `roll`
signs the login with the instance's own credentials.

```sh
roll secrets identities add web --aws arn:aws:iam::123456789012:role/web --for-env production
```

The same works for an ECS task role, EKS's IRSA or Pod Identity. See ["AWS IAM"](../secrets.md#aws-iam).

### Google Compute Engine

Google signs for everyone, so the subject is the service account's numeric unique id:

```sh
gcloud iam service-accounts describe web@acme.iam.gserviceaccount.com --format='value(uniqueId)'
roll secrets identities add web --issuer https://accounts.google.com --subject 104…1 --for-env production
```

On the VM, set `ROLL_IDENTITY_KIND=gcp` next to `ROLL_IDENTITY` in the unit's `Environment=`. See
["On a VM"](../secrets.md#on-a-vm).

### Azure VMs

1. Register an application in Entra ID with an Application ID URI, for example
   `api://roll-secrets`. Entra issues tokens only for a resource it knows.
2. Make an identity for the VM's managed identity, by its object id:

   ```sh
   roll secrets identities add web --issuer https://sts.windows.net/<tenant id>/ \
     --subject <object id> --audience api://roll-secrets --for-env production
   ```

3. On the VM, set `ROLL_IDENTITY_KIND=azure` and `ROLL_OIDC_AUDIENCE=api://roll-secrets` next to
   `ROLL_IDENTITY`. For a user-assigned identity, also set `ROLL_AZURE_CLIENT_ID`.

See ["On a VM"](../secrets.md#on-a-vm).

### Bare metal and on premises

No platform vouches for the machine, so an admin does, once per machine, with a bootstrap token.
The machine then keeps its own short-lived credential.

1. Make a machine identity:

   ```sh
   roll secrets identities add racks --machine --for-env production
   ```

2. Make a bootstrap token for each machine. It's shown once, works once, and expires after 10
   minutes by default:

   ```sh
   roll secrets identities bootstrap idn_…
   ```

   Hand it to the machine's provisioning (cloud-init, Ansible), never to a repository.

3. Put the token in a file the unit reads only on its first start:

   ```ini
   [Service]
   Environment=ROLL_API_URL=https://roll.example.com ROLL_IDENTITY=idn_…
   # /etc/roll/bootstrap.env holds ROLL_BOOTSTRAP_TOKEN=rlbt_…; a new token re-enrolls the machine.
   EnvironmentFile=-/etc/roll/bootstrap.env
   ExecStart=/usr/local/bin/roll secrets -p shop -e production agent --token-file /run/roll/agent-token
   User=shop
   StateDirectory=roll
   RuntimeDirectory=roll
   ```

The first login exchanges the token for a machine credential, which `roll` keeps in the unit's
state directory and replaces on its own. A token or credential used twice revokes the machine. See
["On a VM"](../secrets.md#on-a-vm).

## Windows

1. Install `roll` for the whole machine, from an administrator's PowerShell. The service can't run
   a `roll.exe` from your profile:

   ```powershell
   & ([scriptblock]::Create((irm https://roll.sh/install.ps1))) -System
   ```

   `winget install --scope machine` and `scoop install -g` work too.

2. Install the local agent as a service, with the login it should use. This example uses a roll
   token. On an Azure VM or bare metal, set the variables from [A VM or a server](#a-vm-or-a-server)
   instead:

   ```powershell
   $env:ROLL_TOKEN = "rlt_…"   # roll secrets tokens create agent --for-env production
   roll --api https://roll.example.com secrets -p shop -e production agent --install-service --prefetch production:/ --token-reader "IIS AppPool\shop"
   sc.exe query roll-secrets-agent
   ```

   `--token-reader` names the account that may read the request token: the app's.

   If someone else made `%ProgramData%\roll` or `%ProgramData%\roll\agent`, the command refuses
   to write there. Remove the directory as an administrator, then run the command again.

3. The app reads values from `http://127.0.0.1:2773`, with the contents of
   `%ProgramData%\roll\agent\agent-token` as its `X-Roll-Token` header.

To remove the service, run `roll secrets agent --uninstall-service`. It stops the service and
deletes its login. The token file stays.

For what the service runs as and where it keeps its files, see ["A local
agent"](../secrets.md#a-local-agent). For what else works differently on Windows, see ["On
Windows"](../secrets.md#on-windows).

## AWS Lambda

roll's Lambda extension answers the function's values on `127.0.0.1:2773`, from a cache. It logs in
as a workload identity with a token STS signs for the function's own role, so the function holds
no roll token.

1. Build the layer and publish the zip that matches the function's architecture:

   ```sh
   cd sdk/lambda-extension && make
   aws lambda publish-layer-version --layer-name roll-secrets-arm64 \
     --zip-file fileb://layer-arm64.zip --compatible-architectures arm64
   ```

2. Turn on outbound identity federation for the account, let the function's role call
   `sts:GetWebIdentityToken` for your roll's audience, and make an identity for the role. The
   steps are in the extension's [README](../../sdk/lambda-extension/README.md), "A workload
   identity through AWS STS".

3. Attach the layer and set the variables. `--environment` replaces all the function's
   variables, so list every one it needs:

   ```sh
   aws lambda update-function-configuration --function-name orders \
     --layers arn:aws:lambda:eu-west-1:111122223333:layer:roll-secrets-arm64:1 \
     --environment 'Variables={ROLL_API_URL=https://roll.example.com,ROLL_IDENTITY=idn_…,ROLL_ENV=production,ROLL_PREFETCH=production:/}'
   ```

4. In the function, read `http://127.0.0.1:2773/secrets/{KEY}` with the header `X-Roll-Token:
   $AWS_SESSION_TOKEN`.

The extension's API and caching are in ["AWS Lambda"](../secrets.md#aws-lambda).

## Local development

Run the dev server with its values in its environment. Nothing is written to disk:

```sh
roll run -- npm run dev
```

When `roll run` doesn't fit the tool, use one of these:

- **A tool that reads only a `.env` file** (Next.js, Vite, Prisma, Compose's `env_file`): mount a
  `.env` that's never on disk. Each read of it gets the current values:

  ```sh
  roll secrets mount .env -e development    # in one terminal; Ctrl-C removes it
  npx prisma migrate dev                    # in another
  ```

  It doesn't work on Windows. See ["A local `.env`"](../secrets.md#a-local-env).
- **Several apps on one machine**: run `roll secrets agent` and read values over HTTP, as on a VM.
  See [A VM or a server](#a-vm-or-a-server).
- **Your shell**: with direnv, put `eval "$(roll secrets export --format shell)"` in the
  directory's `.envrc`. See ["The shell"](../secrets.md#the-shell).
- **Docker builds and Compose**: run them under `roll run`. See ["Docker builds and
  Compose"](../secrets.md#docker-builds-and-compose).
- **A committed config with no values**: write `roll://` references in it and run with `roll run
  --env-file`. See ["References in files"](../secrets.md#references-in-files).
- **VS Code**: the extension shows which environments set each variable and moves values from a
  `.env` into roll. See ["VS Code"](../secrets.md#vs-code).
- **Working offline**: `roll run --fallback FILE` starts from an encrypted copy when roll can't be
  reached. See ["Running through an outage"](../secrets.md#running-through-an-outage).
