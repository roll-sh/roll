//go:build scylla

// ScyllaDB's gocql fork: its own metadata types, LWT marking, shard-aware pools and event bus.
package main

import (
	"fmt"
	"strings"
	"time"

	"github.com/gocql/gocql"
	"github.com/gocql/gocql/events"
)

func colType(t string) string {
	if t == "varchar" {
		return "text"
	}
	if strings.HasPrefix(t, "tuple") {
		return "frozen<" + t + ">"
	}
	return t
}

func checkMetadata(s *gocql.Session, types []typ) {
	check("metadata.table", func() error {
		m, err := meta(s, func(m *gocql.KeyspaceMetadata) bool { return m.Tables["types"] != nil })
		if err != nil {
			return err
		}
		if err := expect(strings.HasSuffix(m.StrategyClass, "NetworkTopologyStrategy"), "strategy %s", m.StrategyClass); err != nil {
			return err
		}
		t := m.Tables["kv"]
		if t == nil {
			names := []string{}
			for n := range m.Tables {
				names = append(names, n)
			}
			return fmt.Errorf("no table kv in %v (types %d)", names, len(m.Types))
		}
		if err := expect(t != nil && len(t.PartitionKey) == 1 && t.PartitionKey[0].Name == "k" && len(t.ClusteringColumns) == 1 && t.ClusteringColumns[0].Name == "c", "keys"); err != nil {
			return err
		}
		if err := expect(t.ClusteringColumns[0].Order == gocql.DESC, "clustering order %v", t.ClusteringColumns[0].ClusteringOrder); err != nil {
			return err
		}
		if err := expect(t.Options.Comment == "drivers", "comment %q", t.Options.Comment); err != nil {
			return err
		}
		if err := expect(t.Columns["v"].Type == "text", "v %s", t.Columns["v"].Type); err != nil {
			return err
		}
		tt := m.Tables["types"]
		for _, x := range types {
			c := tt.Columns["c_"+x.label]
			if c == nil || c.Type != colType(x.cql) {
				return fmt.Errorf("%s: %v != %s", x.label, c, colType(x.cql))
			}
		}
		return expect(strings.Contains(m.CreateStmts, "CLUSTERING ORDER BY (c DESC)"), "CreateStmts %q", m.CreateStmts)
	})
	check("metadata.udt", func() error {
		m, err := meta(s, func(m *gocql.KeyspaceMetadata) bool { return m.Types["addr"] != nil })
		if err != nil {
			return err
		}
		u := m.Types["addr"]
		return expect(u != nil && strings.Join(u.FieldNames, ",") == "street,zip,tags" && u.FieldTypes[2] == "frozen<set<text>>", "udt %+v", u)
	})
	check("metadata.index", func() error {
		m, err := meta(s, func(m *gocql.KeyspaceMetadata) bool { return m.Indexes["kv_v"] != nil })
		if err != nil {
			return err
		}
		i := m.Indexes["kv_v"]
		return expect(i != nil && i.TableName == "kv" && i.Kind == "COMPOSITES" && i.Options["target"] == "v", "index %+v", i)
	})
	check("metadata.view", func() error {
		if !features && dialect == "cassandra" {
			return skip("materialized views are disabled")
		}
		if err := s.Query("CREATE MATERIALIZED VIEW kv_by_v AS SELECT k, c, v FROM kv WHERE v IS NOT NULL AND k IS NOT NULL AND c IS NOT NULL PRIMARY KEY (v, k, c)").Exec(); err != nil {
			return err
		}
		var v *gocql.ViewMetadata
		err := eventually(10*time.Second, func() bool {
			m, err := s.KeyspaceMetadata(ks)
			if err == nil {
				v = m.Views["kv_by_v"]
			}
			return v != nil
		})
		if err != nil {
			return err
		}
		return expect(v.BaseTableName == "kv" && len(v.PartitionKey) == 1 && v.PartitionKey[0].Name == "v" && len(v.ClusteringColumns) == 2, "view %+v", v)
	})
	check("metadata.functions", func() error {
		if !features {
			return skip("UDFs are disabled")
		}
		if err := createFunctions(s); err != nil {
			return err
		}
		m, err := meta(s, func(m *gocql.KeyspaceMetadata) bool { return m.Aggregates["total"] != nil })
		if err != nil {
			return fmt.Errorf("%v (functions %v, aggregates %v)", err, keys(m, true), keys(m, false))
		}
		f := m.Functions["twice"]
		lang := map[string]string{"scylla": "lua", "cassandra": "java"}[dialect]
		if err := expect(f != nil && f.ArgumentNames[0] == "x" && f.ReturnType == "int" && f.Language == lang, "function %+v", f); err != nil {
			return err
		}
		a := m.Aggregates["total"]
		return expect(a.StateFunc.Name == "plus" && a.StateType == "int" && a.InitCond == "0", "aggregate %+v", a)
	})
}

func checkLWTMarking(s *gocql.Session) error {
	if dialect != "scylla" {
		return nil
	}
	q := s.Query("UPDATE lwt SET v = ? WHERE k = ? IF n = ?", "x", 1, 99)
	if _, err := q.MapScanCAS(map[string]any{}); err != nil {
		return err
	}
	if err := expect(q.IsLWT(), "prepared LWT marking"); err != nil {
		return err
	}
	q2 := s.Query("SELECT v FROM lwt WHERE k = ?", 1)
	q2.Iter().Close()
	return expect(!q2.IsLWT(), "non-LWT marked")
}

func typeSupported(label string) error { return nil }

func checkShards(s *gocql.Session) error {
	hosts := s.GetHosts()
	if err := expect(len(hosts) == 1, "hosts %v", hosts); err != nil {
		return err
	}
	h := hosts[0]
	if dialect == "cassandra" {
		return expect(h.ScyllaShardCount() == 0, "Cassandra has no shards")
	}
	if err := expect(h.ScyllaShardCount() == shards && h.ScyllaShardAwarePort() == 19042, "shards %d port %d", h.ScyllaShardCount(), h.ScyllaShardAwarePort()); err != nil {
		return err
	}
	pool := s.GetHostPoolByID(h.HostID())
	if pool == nil {
		return fmt.Errorf("no pool for %s", h.HostID())
	}
	err := eventually(15*time.Second, func() bool { return pool.GetShardCount() == shards && pool.GetConnectionCount() >= shards })
	return expect(err == nil, "pool: %d shards, %d connections (%v)", pool.GetShardCount(), pool.GetConnectionCount(), err)
}

func checkEvents(s *gocql.Session) error {
	sub := s.SubscribeToEvents("suite", 64, nil)
	defer sub.Stop()
	other, err := newCluster(user, pass).CreateSession()
	if err != nil {
		return err
	}
	defer other.Close()
	want := func(change string) error {
		deadline := time.After(15 * time.Second)
		for {
			select {
			case e := <-sub.Events():
				if t, ok := e.(*events.SchemaChangeTableEvent); ok && t.Keyspace == ks && t.Table == "evt" && t.Change == change {
					return nil
				}
			case <-deadline:
				return fmt.Errorf("no %s event", change)
			}
		}
	}
	for _, step := range []struct{ q, change string }{
		{"CREATE TABLE " + ks + ".evt (k int PRIMARY KEY, v text)", "CREATED"},
		{"ALTER TABLE " + ks + ".evt ADD w int", "UPDATED"},
		{"DROP TABLE " + ks + ".evt", "DROPPED"},
	} {
		if err := other.Query(step.q).Exec(); err != nil {
			return err
		}
		if err := want(step.change); err != nil {
			return err
		}
	}
	return nil
}

func keys(m *gocql.KeyspaceMetadata, functions bool) []string {
	var out []string
	if m == nil {
		return out
	}
	if functions {
		for k := range m.Functions {
			out = append(out, k)
		}
	} else {
		for k := range m.Aggregates {
			out = append(out, k)
		}
	}
	return out
}
