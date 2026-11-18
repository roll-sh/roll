#!/usr/bin/env bash
# Builds the roll Terraform provider and generates Pulumi SDKs for it, through Pulumi's
# terraform-provider package (sdk/pulumi/README.md). No registry: the SDKs name the binary's path.
#
#   sdk/pulumi/sdk.sh [OUT]     OUT/bin/terraform-provider-roll and OUT/sdk/{nodejs,python,go}
#
# OUT defaults to sdk/pulumi/out. ROLL_PULUMI_LANGUAGES picks the languages (nodejs python go
# dotnet java; default nodejs python go). ROLL_TF_PROVIDER copies a provider already built.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
. "$here/lib.sh"
out=${1:-$here/out}
mkdir -p "$out"
out=$(cd "$out" && pwd)
provider_binary "$out/bin/terraform-provider-roll"
for language in ${ROLL_PULUMI_LANGUAGES:-nodejs python go}; do
  pulumi package gen-sdk terraform-provider --language "$language" -o "$out/sdk" -- "$out/bin/terraform-provider-roll"
done
echo "SDKs in $out/sdk; they load $out/bin/terraform-provider-roll"
