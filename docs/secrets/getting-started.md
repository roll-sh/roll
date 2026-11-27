# Getting started with roll secrets

This guide takes you from nothing to a command that runs with its secrets. You install the CLI, log
in, set up a project, add values, run a command with them, and then hand them to CI. The dashboard
does everything the CLI does, so you can follow along in either.

The rules behind each step are in the reference, [docs/secrets.md](../secrets.md).

## Before you start

You need:

- the address of your roll server. roll has no hosted address yet, so ask whoever runs yours, or
  start one on your laptop (next section).
- an account on it, or a project someone has made you a member of.

## 0. No server yet?

To try roll secrets, run one rolld on your own machine. It keeps everything in SQLite in one
directory, listens on `127.0.0.1` only, and asks for a token of its own, so neither another user of
the machine nor a web page you open can use it.

Download rolld with the CLI (Linux on x86_64 and ARM64, macOS on Apple silicon). The install script
checks both against the release's `SHA256SUMS` and puts them in `~/.local/bin`:

```sh
curl -fsSL https://roll.sh/install.sh | sh -s -- --with-rolld
```

Then start it with a key and an operator token of its own:

```sh
mkdir -p ~/roll-eval && chmod 700 ~/roll-eval
openssl rand -hex 32 > ~/roll-eval/secrets.key && openssl rand -hex 32 > ~/roll-eval/admin.token
chmod 600 ~/roll-eval/secrets.key ~/roll-eval/admin.token
ROLL_SECRET_KEY="$(cat ~/roll-eval/secrets.key)" ROLL_ADMIN_TOKEN="$(cat ~/roll-eval/admin.token)" \
  ROLL_HOME=~/roll-eval/home ROLL_PRODUCTS=tasks rolld --api 127.0.0.1:7070
```

If `~/.local/bin` isn't on your `PATH` yet, run `~/.local/bin/rolld` instead (the script tells you
what to add to your profile). On Windows, run rolld in WSL: it has no Windows build. On an Intel
Mac, build it from a clone (below): it has no Intel macOS build.

And in the shell you'll use the CLI from:

```sh
export ROLL_ADMIN_TOKEN="$(cat ~/roll-eval/admin.token)"
```

Or build it from a clone, with a current stable Rust (the first build takes a while):

```sh
git clone https://github.com/roll-sh/roll && cd roll
cargo build --release -p rolld -p roll-cli
```

and start it with `./target/release/rolld` in place of `rolld` above. The build gives you the CLI
too (`./target/release/roll`).

- `ROLL_SECRET_KEY` encrypts every value. Keep the file: without it, the values can't be read
  again.
- roll secrets is always on. `ROLL_PRODUCTS` must name at least one product, and `tasks` opens no
  port of its own, so rolld listens on `127.0.0.1:7070` only: no Postgres, no Docker.
- `ROLL_ADMIN_TOKEN` is the operator's token: every command that carries it runs as the operator.
  Without one, rolld on `127.0.0.1` is open to every user and process on the machine. It still
  answers only requests addressed to `localhost`, `127.0.0.1`, `[::1]` or a name under
  `.localhost` (`api.localhost`, which browsers send to loopback themselves), so a web page can't
  reach it by pointing a name of its own at your machine. Any other name gets 421 (a proxy,
  `tailscale serve`, `ssh -R` to a name): set `ROLL_DASHBOARD_ORIGIN` to the URL you open it at,
  or set the token. Keep the token for anything you import real values into.

You have the CLI already, so skip step 1, and skip step 2 as well: `roll login` signs a person in
through the dashboard, and this node has no dashboard and no accounts. With no login, the CLI goes
to a local rolld on `127.0.0.1:7070`, so there's nothing to set. On another port,
`export ROLL_API_URL=http://127.0.0.1:<port>`.

Then, in your repository:

```sh
roll secrets setup -p shop -e development --create
roll secrets set STRIPE_KEY=sk_test_example LOG_LEVEL=debug
roll run -- sh -c 'echo "LOG_LEVEL is $LOG_LEVEL"'
```

Steps 3 to 7 work the same way. To stop, press Ctrl-C in rolld's terminal. `rm -rf ~/roll-eval`
deletes everything it made, and `roll secrets setup` kept this directory's choice in
`~/.config/roll/secrets.json`.

For a server your team shares, with accounts, Postgres and several nodes, see ["Where it
runs"](../secrets.md#where-it-runs).

## 1. Install the CLI

**Linux and macOS** (x86_64 and ARM64):

```sh
curl -fsSL https://roll.sh/install.sh | sh
```

The script picks the build for your system, checks it against the release's `SHA256SUMS`, and puts
`roll` in `~/.local/bin`. Set `ROLL_INSTALL_DIR` to put it somewhere else, or `ROLL_VERSION` to
install one release. If `~/.local/bin` isn't on your `PATH`, the script tells you what to add.

**Windows** (x86_64 and ARM64), in PowerShell:

```powershell
irm https://roll.sh/install.ps1 | iex
```

This puts `roll.exe` in `%LOCALAPPDATA%\Programs\roll` and on your `PATH`. Every release also has
winget and Scoop manifests. To install for every account on a server, or to learn what works
differently on Windows, see ["On Windows"](../secrets.md#on-windows).

Check the install:

```sh
roll --version
```

## 2. Log in

```sh
roll login https://roll.example.com
```

`roll login` opens your browser. Over SSH, it shows a code to enter in a browser somewhere else.
Later commands go to the same roll, so you don't need to set `ROLL_API_URL`.

The first time you reveal a value, roll asks you to approve it. The first `roll run` prints where
to approve and waits for you.

## 3. Set up a project for this directory

A workspace project gets `development`, `staging` and `production` the first time you open its
secrets. `production` is protected from the start.

In your repository, run:

```sh
roll secrets setup -p shop -e development
```

If `shop` doesn't exist yet, `setup` offers to create it (`--create` makes it without asking). roll remembers the project and environment
for this directory and the ones under it, so later commands need no flags. With no flags, `setup`
lists the projects and environments you can see and lets you pick.

To suggest the same setup to your teammates, add a `[secrets]` table to the repository's
`roll.toml`:

```toml
[secrets]
project = "shop"
environment = "development"
branches = { "release/*" = "staging" }
```

Each teammate runs `roll secrets setup` once and approves the table. The table never chooses a
protected environment. See ["The repo's suggestion"](../secrets.md#roll-secrets-and-roll-run).

Run `roll secrets doctor` at any time to check the setup.

## 4. Add values

Set values one at a time, or import a `.env` file as one change:

```sh
roll secrets set STRIPE_KEY=sk_test_… LOG_LEVEL=debug
roll secrets import .env
```

A value can refer to other values, in this environment or another:

```sh
roll secrets set DATABASE_URL='postgres://app:${PASSWORD}@${staging.DB_HOST}/shop'
```

See ["References"](../secrets.md#references).

Coming from another secrets manager? [Moving from another manager](moving-from.md) imports from
its API.

## 5. Link roll resources

Don't paste the credentials of a roll resource. Link the key to the resource instead, and the value
follows it when the password rotates or a new key is issued. In the dashboard, open the
environment and choose **Link a resource**: a Postgres branch's URL, a Redis URL, a bucket's own
access key, a stream's API key or a search API key. See ["Links"](../secrets.md#links).

## 6. Run a command with its values

```sh
roll run -- npm run dev
```

`roll run` adds the environment's values to the command's environment. Nothing is written to disk.
To restart the command when a value changes, add `--watch`:

```sh
roll run -e production --watch -- ./server
```

`roll run` masks the values in what the command prints to a log. For the details, and for every
other `roll secrets` command, see ["`roll secrets` and `roll run`"](../secrets.md#roll-secrets-and-roll-run).

For tools that only read a `.env` file, see [Local development](platforms.md#local-development).

## 7. Promote and protect

Compare two environments, then promote what changed:

```sh
roll secrets diff --from development --to staging
roll secrets promote --from staging --to production STRIPE_KEY
```

Require an approval for every write to production:

```sh
roll secrets envs protect production --approvals 1
```

Writes to production then become change requests that someone else approves. See ["Protected
environments and approvals"](../secrets.md#protected-environments-and-approvals).

## 8. Give CI and services their values

The best way is a workload identity: the job or the machine logs in with its own short-lived token,
so no roll token sits in its settings. [Platforms](platforms.md) shows how for each CI system,
Kubernetes, VMs, Windows and AWS Lambda.

Where a platform has no identity of its own, make a roll token with one rule on the project:

```sh
roll secrets tokens create ci --for-env staging
ROLL_TOKEN=rlt_… roll run -e staging -- make test
```

By default the token reads one environment and expires after 90 days. See
["Access"](../secrets.md#access).

To push an environment to a platform instead (GitHub Actions secrets, Vercel, AWS), add a
[sync](../secrets.md#syncs). roll's own checks, previews, apps and sandboxes read their environment
directly.

To read values from code, use a client: TypeScript, Go, Python, Java, .NET or Ruby. See
["Clients"](../secrets.md#clients).

## 9. Use the dashboard

Open a project from the dashboard's sidebar, then **Secrets**:

- **Overview** shows every key across the environments, and marks the keys whose values differ.
- One tab per environment holds its folders and keys: **Add secret**, **Link a resource**, **Import**
  and **Export**.
- **Health** lists what needs fixing: expired, missing or exposed keys.
- **Compare** shows two environments side by side and promotes between them.
- **Environments** adds, protects, orders and removes environments.
- **Access** holds tokens, workload identities, leases, shared links and requests for a value.
- **Syncs**, **Credentials** and **Keys & certificates** hold what the project pushes, rotates and
  issues.
- **Events** and **Audit** show what happened, and who did it.

## When something goes wrong

- `roll secrets history` lists the changes, and `roll secrets rollback --at 2h` puts an environment
  back as it was two hours ago. See ["Changes and versions"](../secrets.md#changes-and-versions).
- `roll secrets audit` shows every read and write. You can stream the audit log to your SIEM. See
  ["Audit log"](../secrets.md#audit-log).
- `roll secrets share KEY` hands one value to someone outside roll, through a link that works once.
  See ["Sharing a value once"](../secrets.md#sharing-a-value-once).
- If a value leaks, see ["When a value leaks"](../secrets.md#when-a-value-leaks).
