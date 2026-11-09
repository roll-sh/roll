//go:build !scylla

// github.com/gocql/gocql's metadata, LWT, shard and event APIs.
package main

import (
	"fmt"
	"strings"
	"time"

	"github.com/gocql/gocql"
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
		if err := expect(t != nil && len(t.PartitionKey) == 1 && t.PartitionKey[0].Name == "k" && len(t.ClusteringColumns) == 1 && t.ClusteringColumns[0].Name == "c", "keys"); err != nil {
			return err
		}
		if err := expect(t.ClusteringColumns[0].Order == gocql.DESC, "clustering order %v", t.ClusteringColumns[0].ClusteringOrder); err != nil {
			return err
		}
		if err := expect(t.Columns["v"].Validator == "text", "v %s", t.Columns["v"].Validator); err != nil {
			return err
		}
		tt := m.Tables["types"]
		for _, x := range types {
			c := tt.Columns["c_"+x.label]
			if c == nil || c.Validator != colType(x.cql) {
				return fmt.Errorf("%s: %v != %s", x.label, c, colType(x.cql))
			}
		}
		return nil
	})
	check("metadata.udt", func() error {
		m, err := meta(s, func(m *gocql.KeyspaceMetadata) bool { return m.UserTypes["addr"] != nil })
		if err != nil {
			return err
		}
		u := m.UserTypes["addr"]
		return expect(u != nil && strings.Join(u.FieldNames, ",") == "street,zip,tags" && u.FieldTypes[2].Type() == gocql.TypeSet, "udt %+v", u)
	})
	check("metadata.index", func() error { return skip("gocql doesn't read index metadata") })
	check("metadata.view", func() error {
		if !features && dialect == "cassandra" {
			return skip("materialized views are disabled")
		}
		if err := s.Query("CREATE MATERIALIZED VIEW kv_by_v AS SELECT k, c, v FROM kv WHERE v IS NOT NULL AND k IS NOT NULL AND c IS NOT NULL PRIMARY KEY (v, k, c)").Exec(); err != nil {
			return err
		}
		m, err := meta(s, func(m *gocql.KeyspaceMetadata) bool { return m.MaterializedViews["kv_by_v"] != nil })
		if err != nil {
			return err
		}
		v := m.MaterializedViews["kv_by_v"]
		return expect(v != nil && v.BaseTable != nil && v.BaseTable.Name == "kv", "view %+v", v)
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
		if err := expect(f != nil && f.ArgumentNames[0] == "x" && f.ReturnType.Type() == gocql.TypeInt && f.Language == lang, "function %+v", f); err != nil {
			return err
		}
		a := m.Aggregates["total"]
		return expect(a != nil && a.StateFunc.Name == "plus" && a.StateType.Type() == gocql.TypeInt && a.InitCond == "0", "aggregate %+v", a)
	})
}

func checkLWTMarking(s *gocql.Session) error { return nil }

func typeSupported(label string) error {
	if label == "vector" {
		return skip("gocql 1.7 has no vector type")
	}
	return nil
}

func checkShards(s *gocql.Session) error { return skip("not a shard-aware driver") }

// gocql has no schema listener: its keyspace metadata cache is dropped when a schema change
// event arrives, so a change made elsewhere shows up only through the event.
func checkEvents(s *gocql.Session) error {
	if _, err := s.KeyspaceMetadata(ks); err != nil {
		return err
	}
	other, err := newCluster(user, pass).CreateSession()
	if err != nil {
		return err
	}
	defer other.Close()
	has := func(table, col string) bool {
		m, err := s.KeyspaceMetadata(ks)
		if err != nil {
			return false
		}
		t, ok := m.Tables[table]
		if col == "" || !ok {
			return ok
		}
		_, ok = t.Columns[col]
		return ok
	}
	if err := other.Query("CREATE TABLE " + ks + ".evt (k int PRIMARY KEY, v text)").Exec(); err != nil {
		return err
	}
	if err := eventually(15*time.Second, func() bool { return has("evt", "") }); err != nil {
		return fmt.Errorf("created table: %v", err)
	}
	if err := other.Query("ALTER TABLE " + ks + ".evt ADD w int").Exec(); err != nil {
		return err
	}
	if err := eventually(15*time.Second, func() bool { return has("evt", "w") }); err != nil {
		return fmt.Errorf("altered table: %v", err)
	}
	if err := other.Query("DROP TABLE " + ks + ".evt").Exec(); err != nil {
		return err
	}
	return eventually(15*time.Second, func() bool { return !has("evt", "") })
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
