// The roll Terraform provider: roll secrets' environments, keys and values as code
// (docs/secrets.md, "Terraform").
package main

import (
	"context"
	"flag"
	"log"

	"github.com/hashicorp/terraform-plugin-framework/providerserver"
	"github.com/roll-sh/roll/sdk/terraform-provider-roll/internal/provider"
)

var version = "dev"

func main() {
	debug := flag.Bool("debug", false, "run with support for debuggers like delve")
	flag.Parse()
	err := providerserver.Serve(context.Background(), provider.New(version), providerserver.ServeOpts{Address: "registry.terraform.io/roll-sh/roll", Debug: *debug})
	if err != nil {
		log.Fatal(err)
	}
}
