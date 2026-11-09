// Testcontainers for Go's cassandra and scylladb modules (both wait for `cqlsh -e "SELECT
// bootstrapped FROM system.local"` to print COMPLETED inside the container) with
// ROLL_NOSQL_IMAGE. Prints `PASS <check>` / `FAIL <check>: <why>`.
package main

import (
	"context"
	"fmt"
	"os"

	"github.com/gocql/gocql"
	"github.com/testcontainers/testcontainers-go"
	"github.com/testcontainers/testcontainers-go/modules/cassandra"
	"github.com/testcontainers/testcontainers-go/modules/scylladb"
)

var failed = 0

func check(name string, f func() error) {
	if err := f(); err != nil {
		failed++
		fmt.Printf("FAIL %s: %v\n", name, err)
		return
	}
	fmt.Printf("PASS %s\n", name)
}

func query(hostPort, release string) error {
	c := gocql.NewCluster(hostPort)
	s, err := c.CreateSession()
	if err != nil {
		return err
	}
	defer s.Close()
	var v string
	if err := s.Query("SELECT release_version FROM system.local").Scan(&v); err != nil {
		return err
	}
	if v != release {
		return fmt.Errorf("release_version %s", v)
	}
	return nil
}

func main() {
	ctx := context.Background()
	img := os.Getenv("ROLL_NOSQL_IMAGE")
	check("testcontainers-go.cassandra", func() error {
		ctr, err := cassandra.Run(ctx, img, cassandra.WithInitScripts("init.cql"))
		defer testcontainers.TerminateContainer(ctr)
		if err != nil {
			return err
		}
		host, err := ctr.ConnectionHost(ctx)
		if err != nil {
			return err
		}
		if err := query(host, "5.0.9"); err != nil {
			return err
		}
		c := gocql.NewCluster(host)
		s, err := c.CreateSession()
		if err != nil {
			return err
		}
		defer s.Close()
		var name string
		if err := s.Query("SELECT name FROM tc.users WHERE id = 1").Scan(&name); err != nil || name != "init script" {
			return fmt.Errorf("init script: %q %v", name, err)
		}
		return nil
	})
	check("testcontainers-go.scylladb", func() error {
		ctr, err := scylladb.Run(ctx, img, scylladb.WithShardAwareness(), scylladb.WithAlternator())
		defer testcontainers.TerminateContainer(ctr)
		if err != nil {
			return err
		}
		host, err := ctr.NonShardAwareConnectionHost(ctx)
		if err != nil {
			return err
		}
		if _, err := ctr.AlternatorConnectionHost(ctx); err != nil {
			return err
		}
		return query(host, "3.0.8")
	})
	if failed > 0 {
		os.Exit(1)
	}
}
