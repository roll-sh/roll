# roll secrets for VS Code

Shows which environments set the environment variables your code reads, warns where the
workspace's environment lacks one, completes names where a name goes, and in a `.env` file turns a
value into a `roll://` reference (moving it into roll first, if you like). It reads only names and
which environments set them; it never shows, logs or keeps a value.

- **Where:** what `roll secrets setup` remembers for the folder (`~/.config/roll/secrets.json`), or
  the `roll.project`, `roll.environment` and `roll.path` settings. The status bar names the project
  and environment; click it to choose another environment for this workspace.
- **Who:** `roll secrets: Sign in` keeps a project token in VS Code's secret storage
  (`roll secrets tokens create vscode --for-env development`), or `ROLL_TOKEN` in the environment.
  `roll.apiUrl` (or `ROLL_API_URL`) is roll's API.

Build with `pnpm --filter roll-secrets build`; the design is in `docs/secrets.md`, "VS Code".
