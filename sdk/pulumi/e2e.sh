#!/usr/bin/env bash
# The end-to-end check: example/ against the Terraform provider's fake rolld (TestServeFakeRolld),
# `pulumi up` then `pulumi destroy` on a local backend in a temporary directory. Needs pulumi,
# node and npm, and go or Docker. Checks that roll got the write-only value, that neither it nor the
# token is in the state in the clear, and that destroy removed what up made and revoked the token.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
. "$here/lib.sh"
work=$(mktemp -d)
fake=
cleanup() {
  [ -z "$fake" ] || kill "$fake" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

project=$work/project
mkdir -p "$project"
cp "$here"/example/{Pulumi.yaml,package.json,tsconfig.json,index.ts} "$project/"
provider_binary "$project/bin/terraform-provider-roll"
go_build "$work/fake.test" test -c ./internal/provider

port=$(node -e 'const s=require("net").createServer().listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close()})')
api=http://127.0.0.1:$port
ROLL_FAKE_ROLLD_ADDR=127.0.0.1:$port "$work/fake.test" -test.run '^TestServeFakeRolld$' -test.timeout 0 >"$work/fake.log" 2>&1 &
fake=$!
for _ in $(seq 50); do curl -fs "$api/fake/state" >/dev/null && break; sleep 0.1; done

# Obviously fake: the fake rolld's token, and a value nothing accepts.
value=fake-stripe-key-for-pulumi-e2e
export PULUMI_BACKEND_URL=file://$work/state PULUMI_CONFIG_PASSPHRASE=not-a-real-passphrase PULUMI_SKIP_UPDATE_CHECK=1
mkdir -p "$work/state"
cd "$project"
pulumi install
pulumi stack init e2e
pulumi config set roll:apiUrl "$api"
pulumi config set roll:project prj_a
pulumi config set --secret roll:token rlt_terraform
pulumi config set --secret stripeKey "$value"
npx --no-install tsc --noEmit
pulumi up --yes --skip-preview

check() { # check NAME JS: JS reads s (the fake's state), state, plain (the state decrypted), outputs
  curl -fs "$api/fake/state" >"$work/fake.json"
  node -e '
    const fs = require("fs"), [, file, w] = process.argv, read = (f) => fs.existsSync(w + f) ? fs.readFileSync(w + f, "utf8") : "";
    const s = JSON.parse(read("/fake.json")), state = read("/state.json"), plain = read("/plain.json"), outputs = read("/outputs.json");
    if (!('"$2"')) { console.error("FAIL: " + file); process.exit(1) }
    console.log("ok: " + file)' "$1" "$work"
}

pulumi stack export >"$work/state.json"
pulumi stack export --show-secrets >"$work/plain.json"
pulumi stack output --json >"$work/outputs.json"
check "roll secrets got the write-only value" 's.keys["staging:/STRIPE_KEY"]?.value === "'"$value"'"'
check "the environment keeps its name" 's.environments.staging?.name === "staging"'
check "the token is live" 'Object.values(s.tokens).length === 1 && Object.values(s.tokens)[0].revoked_at === null'
check "no value or token in the state in the clear" '!state.includes("'"$value"'") && !state.includes("rlt_made_")'
# Pulumi keeps every input in the state, so valueWo is there, encrypted; the provider never reads it back.
check "the key's outputs hold no value" '!JSON.stringify(JSON.parse(plain).deployment.resources.filter((r) => r.type === "roll:index/secret:Secret").map((r) => r.outputs)).includes("'"$value"'")'
check "token and value outputs are secrets" 'JSON.parse(outputs).token === "[secret]" && JSON.parse(outputs).stripeKey === "[secret]"'

pulumi destroy --yes --skip-preview
check "destroy removed the environment and key and revoked the token" \
  'Object.keys(s.environments).length === 0 && Object.keys(s.keys).length === 0 && Object.values(s.tokens).every((t) => t.revoked_at)'
pulumi stack rm --yes e2e
echo "pulumi e2e: ok"
