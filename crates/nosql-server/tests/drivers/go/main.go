// The driver suite for Go: github.com/gocql/gocql and ScyllaDB's fork (built with -tags scylla;
// meta_*.go hold the parts whose API differs). Same checks, names, environment and output as
// tests/drivers/python/suite.py: `PASS <name>`, `FAIL <name>: <why>`, `SKIP <name>: <why>`.
package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"math"
	"math/big"
	"net"
	"os"
	"reflect"
	"runtime/debug"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/gocql/gocql"
	"gopkg.in/inf.v0"
)

var (
	host      = env("HOST", "127.0.0.1")
	port, _   = strconv.Atoi(env("PORT", "9042"))
	dialect   = env("DIALECT", "scylla")
	dc        = env("DC", "datacenter1")
	user      = env("USER_NAME", "")
	pass      = env("PASS", "")
	features  = env("FEATURES", "") == "1"
	shards, _ = strconv.Atoi(env("SHARDS", "0"))
	restart   = env("RESTART", "") == "1"
	ks        = env("KS", "drv_go")
	failed    = 0
)

type skip string

func env(k, d string) string {
	if v, ok := os.LookupEnv(k); ok {
		return v
	}
	return d
}

func out(line string) { fmt.Println(line) }

func check(name string, f func() error) {
	defer func() {
		if r := recover(); r != nil {
			failed++
			out(fmt.Sprintf("FAIL %s: panic: %v", name, r))
			if os.Getenv("VERBOSE") != "" {
				os.Stderr.Write(debug.Stack())
			}
		}
	}()
	err := f()
	var sk skip
	switch {
	case err == nil:
		out("PASS " + name)
	case errors.As(err, &sk):
		out("SKIP " + name + ": " + string(sk))
	default:
		failed++
		msg := strings.ReplaceAll(err.Error(), "\n", " ")
		if len(msg) > 400 {
			msg = msg[:400]
		}
		out("FAIL " + name + ": " + msg)
	}
}

func (s skip) Error() string { return string(s) }

func expect(cond bool, format string, args ...any) error {
	if cond {
		return nil
	}
	return fmt.Errorf(format, args...)
}

func eventually(timeout time.Duration, f func() bool) error {
	end := time.Now().Add(timeout)
	for !f() {
		if time.Now().After(end) {
			return errors.New("timed out")
		}
		time.Sleep(200 * time.Millisecond)
	}
	return nil
}

// meta polls the keyspace's metadata until cond holds: gocql caches it and drops the cache
// when a schema change event arrives (debounced), so it trails DDL by up to a second or so.
func meta(s *gocql.Session, cond func(*gocql.KeyspaceMetadata) bool) (*gocql.KeyspaceMetadata, error) {
	var m *gocql.KeyspaceMetadata
	var err error
	end := time.Now().Add(15 * time.Second)
	for {
		m, err = s.KeyspaceMetadata(ks)
		if err == nil && cond(m) {
			return m, nil
		}
		if time.Now().After(end) {
			if err == nil {
				err = errors.New("timed out waiting for metadata")
			}
			return m, err
		}
		time.Sleep(200 * time.Millisecond)
	}
}

func newCluster(u, p string) *gocql.ClusterConfig {
	c := gocql.NewCluster(host)
	c.Port = port
	c.Timeout = 30 * time.Second
	c.ConnectTimeout = 10 * time.Second
	c.ReconnectInterval = time.Second
	c.PoolConfig.HostSelectionPolicy = gocql.TokenAwareHostPolicy(gocql.DCAwareRoundRobinPolicy(dc))
	if u != "" {
		c.Authenticator = gocql.PasswordAuthenticator{Username: u, Password: p}
	}
	return c
}

// murmur3 is Cassandra's Murmur3Partitioner token of a partition key: MurmurHash3 x64_128's
// first half, with Cassandra's sign extension of the tail bytes.
func murmur3(data []byte) int64 {
	const c1, c2 = uint64(0x87c37b91114253d5), uint64(0x4cf5ad432745937f)
	rotl := func(x uint64, r uint) uint64 { return x<<r | x>>(64-r) }
	fmix := func(k uint64) uint64 {
		k ^= k >> 33
		k *= 0xff51afd7ed558ccd
		k ^= k >> 33
		k *= 0xc4ceb9fe1a85ec53
		k ^= k >> 33
		return k
	}
	n := len(data)
	var h1, h2 uint64
	for i := 0; i+16 <= n; i += 16 {
		k1 := binary.LittleEndian.Uint64(data[i:])
		k2 := binary.LittleEndian.Uint64(data[i+8:])
		k1 *= c1
		k1 = rotl(k1, 31)
		k1 *= c2
		h1 ^= k1
		h1 = rotl(h1, 27)
		h1 += h2
		h1 = h1*5 + 0x52dce729
		k2 *= c2
		k2 = rotl(k2, 33)
		k2 *= c1
		h2 ^= k2
		h2 = rotl(h2, 31)
		h2 += h1
		h2 = h2*5 + 0x38495ab5
	}
	tail := data[n/16*16:]
	b := func(i int) uint64 { return uint64(int64(int8(tail[i]))) }
	var k1, k2 uint64
	rest := n & 15
	for i := rest - 1; i >= 8; i-- {
		k2 ^= b(i) << (uint(i-8) * 8)
	}
	if rest > 8 {
		k2 *= c2
		k2 = rotl(k2, 33)
		k2 *= c1
		h2 ^= k2
	}
	for i := min(rest, 8) - 1; i >= 0; i-- {
		k1 ^= b(i) << (uint(i) * 8)
	}
	if rest > 0 {
		k1 *= c1
		k1 = rotl(k1, 31)
		k1 *= c2
		h1 ^= k1
	}
	h1 ^= uint64(n)
	h2 ^= uint64(n)
	h1 += h2
	h2 += h1
	h1 = fmix(h1)
	h2 = fmix(h2)
	h1 += h2
	t := int64(h1)
	if t == math.MinInt64 {
		t = math.MaxInt64
	}
	return t
}

type addr struct {
	Street string   `cql:"street"`
	Zip    int      `cql:"zip"`
	Tags   []string `cql:"tags"`
}

type typ struct {
	label, cql string
	value      any
	dest       func() any
}

func main() {
	var s *gocql.Session
	check("connect", func() error {
		var err error
		s, err = newCluster(user, pass).CreateSession()
		return err
	})
	if s == nil {
		os.Exit(1)
	}

	check("auth.bad_password", func() error {
		if user == "" {
			return skip("no authenticator")
		}
		bad, err := newCluster(user, "wrong-password").CreateSession()
		if err == nil {
			bad.Close()
			return errors.New("connected with a wrong password")
		}
		m := strings.ToLower(err.Error())
		return expect(strings.Contains(m, "password") || strings.Contains(m, "authenticat"), "error %v", err)
	})

	check("system.local", func() error {
		var rv, d, rack string
		iter := s.Query("SELECT release_version, data_center, rack FROM system.local").Iter()
		iter.Scan(&rv, &d, &rack)
		h := iter.Host()
		if err := iter.Close(); err != nil {
			return err
		}
		want := map[string]string{"scylla": "3.0.8", "cassandra": "5.0.9"}[dialect]
		if err := expect(rv == want && d == dc, "release_version %s data_center %s", rv, d); err != nil {
			return err
		}
		return expect(h != nil && h.DataCenter() == dc && h.Rack() == rack, "host %v", h)
	})

	must := func(q string, args ...any) {
		if err := s.Query(q, args...).Exec(); err != nil {
			out(fmt.Sprintf("FAIL setup: %s: %v", q, err))
			os.Exit(1)
		}
	}
	must("DROP KEYSPACE IF EXISTS " + ks)
	must(fmt.Sprintf("CREATE KEYSPACE %s WITH replication = {'class': 'NetworkTopologyStrategy', '%s': 1}", ks, dc))
	s.Close()
	cl := newCluster(user, pass)
	cl.Keyspace = ks
	var err error
	if s, err = cl.CreateSession(); err != nil {
		out("FAIL setup: " + err.Error())
		os.Exit(1)
	}
	must("CREATE TYPE addr (street text, zip int, tags frozen<set<text>>)")
	must("CREATE TABLE kv (k int, c int, v text, PRIMARY KEY (k, c)) WITH CLUSTERING ORDER BY (c DESC) AND comment = 'drivers'")
	must("CREATE TABLE counters (k int PRIMARY KEY, n counter)")
	must("CREATE TABLE lwt (k int PRIMARY KEY, v text, n int)")
	must("CREATE INDEX kv_v ON kv (v)")

	dec, _ := new(inf.Dec).SetString("-1234567890.0987654321")
	vi, _ := new(big.Int).SetString("123456789012345678901234567890", 10)
	tu, _ := gocql.ParseUUID("d2177dd0-eaa2-11de-a572-001b779c76e3")
	uu, _ := gocql.ParseUUID("01234567-89ab-cdef-0123-456789abcdef")
	types := []typ{
		{"ascii", "ascii", "plain ascii", func() any { return new(string) }},
		{"bigint", "bigint", int64(math.MinInt64), func() any { return new(int64) }},
		{"blob", "blob", []byte{0, 1, 0xfe, 0xff}, func() any { return new([]byte) }},
		{"boolean", "boolean", true, func() any { return new(bool) }},
		{"date", "date", time.Date(2024, 2, 29, 0, 0, 0, 0, time.UTC), func() any { return new(time.Time) }},
		{"decimal", "decimal", dec, func() any { return new(*inf.Dec) }},
		{"double", "double", 1.5e300, func() any { return new(float64) }},
		{"duration", "duration", gocql.Duration{Months: 1, Days: 2, Nanoseconds: 3_000_000_004}, func() any { return new(gocql.Duration) }},
		{"float", "float", float32(0.5), func() any { return new(float32) }},
		{"inet4", "inet", net.ParseIP("192.168.1.10").To4(), func() any { return new(net.IP) }},
		{"inet6", "inet", net.ParseIP("2001:db8::1"), func() any { return new(net.IP) }},
		{"int", "int", int(math.MinInt32), func() any { return new(int) }},
		{"smallint", "smallint", int16(math.MinInt16), func() any { return new(int16) }},
		{"text", "text", "texte unicode é中\U0001f600", func() any { return new(string) }},
		{"time", "time", 23*time.Hour + 59*time.Minute + 59*time.Second + 999999999, func() any { return new(time.Duration) }},
		{"timestamp", "timestamp", time.Date(2026, 10, 20, 8, 0, 1, 234000000, time.UTC), func() any { return new(time.Time) }},
		{"timeuuid", "timeuuid", tu, func() any { return new(gocql.UUID) }},
		{"tinyint", "tinyint", int8(math.MinInt8), func() any { return new(int8) }},
		{"uuid", "uuid", uu, func() any { return new(gocql.UUID) }},
		{"varchar", "varchar", "varchar", func() any { return new(string) }},
		{"varint", "varint", vi, func() any { return new(*big.Int) }},
		{"list", "list<int>", []int{3, 1, 2}, func() any { return new([]int) }},
		{"set", "set<text>", []string{"a", "b"}, func() any { return new([]string) }},
		{"map", "map<text, int>", map[string]int{"x": 1, "y": 2}, func() any { return new(map[string]int) }},
		{"nested", "frozen<list<frozen<map<int, text>>>>", []map[int]string{{1: "one"}, {2: "two"}}, func() any { return new([]map[int]string) }},
		{"tuple", "tuple<int, text, double>", nil, nil},
		{"udt", "frozen<addr>", addr{"Main St", 12345, []string{"home", "work"}}, func() any { return new(addr) }},
		{"udt_nonfrozen", "addr", addr{"Main St", 12345, []string{"home", "work"}}, func() any { return new(addr) }},
		{"vector", "vector<float, 3>", []float32{0.5, 1.0, -2.0}, func() any { return new([]float32) }},
	}
	cols := ""
	for _, t := range types {
		cols += fmt.Sprintf(", c_%s %s", t.label, t.cql)
	}
	must("CREATE TABLE types (k int PRIMARY KEY" + cols + ")")

	checkMetadata(s, types)

	check("schema_agreement", func() error {
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		if err := s.AwaitSchemaAgreement(ctx); err != nil {
			return err
		}
		if err := s.Query("CREATE TABLE agree (k int PRIMARY KEY)").Exec(); err != nil {
			return err
		}
		return s.AwaitSchemaAgreement(ctx)
	})

	check("prepared", func() error {
		for i := 0; i < 3; i++ {
			if err := s.Query("INSERT INTO kv (k, c, v) VALUES (?, ?, ?)", 1, i, fmt.Sprintf("v%d", i)).Exec(); err != nil {
				return err
			}
		}
		q := s.Query("SELECT c, v FROM kv WHERE k = :k AND c >= :lo").Bind(1, 1)
		rk, err := q.GetRoutingKey()
		if err != nil {
			return err
		}
		if err := expect(bytes.Equal(rk, []byte{0, 0, 0, 1}), "routing key %x", rk); err != nil {
			return err
		}
		iter := q.Iter()
		var got []string
		var c int
		var v string
		for iter.Scan(&c, &v) {
			got = append(got, fmt.Sprintf("%d=%s", c, v))
		}
		if err := iter.Close(); err != nil {
			return err
		}
		cols := iter.Columns()
		if err := expect(len(cols) == 2 && cols[0].Name == "c" && cols[1].TypeInfo.Type() == gocql.TypeVarchar, "result metadata %v", cols); err != nil {
			return err
		}
		return expect(strings.Join(got, ",") == "2=v2,1=v1", "rows %v", got)
	})

	check("paging", func() error {
		for i := 0; i < 250; i++ {
			if err := s.Query("INSERT INTO kv (k, c, v) VALUES (2, ?, 'p')", i).Exec(); err != nil {
				return err
			}
		}
		iter := s.Query("SELECT c FROM kv WHERE k = 2").PageSize(50).Iter()
		if err := expect(iter.NumRows() == 50 && len(iter.PageState()) > 0, "first page %d", iter.NumRows()); err != nil {
			return err
		}
		state := iter.PageState()
		iter.Close()
		var c int
		it2 := s.Query("SELECT c FROM kv WHERE k = 2").PageSize(50).PageState(state).Iter()
		it2.Scan(&c)
		it2.Close()
		if err := expect(c == 199, "resumed page starts at %d", c); err != nil {
			return err
		}
		all := s.Query("SELECT c FROM kv WHERE k = 2").PageSize(50).Iter()
		n, first := 0, -1
		for all.Scan(&c) {
			if first < 0 {
				first = c
			}
			n++
		}
		if err := all.Close(); err != nil {
			return err
		}
		return expect(n == 250 && first == 249 && c == 0, "%d rows", n)
	})

	check("batches", func() error {
		b := s.NewBatch(gocql.LoggedBatch)
		b.Query("INSERT INTO kv (k, c, v) VALUES (?, ?, ?)", 3, 1, "b1")
		b.Query("INSERT INTO kv (k, c, v) VALUES (4, 1, 'b2')")
		if err := s.ExecuteBatch(b); err != nil {
			return err
		}
		u := s.NewBatch(gocql.UnloggedBatch)
		u.Query("INSERT INTO kv (k, c, v) VALUES (?, ?, ?)", 3, 2, "u1")
		u.Query("INSERT INTO kv (k, c, v) VALUES (?, ?, ?)", 3, 3, "u2")
		if err := s.ExecuteBatch(u); err != nil {
			return err
		}
		cb := s.NewBatch(gocql.CounterBatch)
		cb.Query("UPDATE counters SET n = n + ? WHERE k = ?", int64(5), 1)
		cb.Query("UPDATE counters SET n = n + ? WHERE k = ?", int64(2), 1)
		if err := s.ExecuteBatch(cb); err != nil {
			return err
		}
		var n int
		if err := s.Query("SELECT count(*) FROM kv WHERE k = 3").Scan(&n); err != nil {
			return err
		}
		var ctr int64
		if err := s.Query("SELECT n FROM counters WHERE k = 1").Scan(&ctr); err != nil {
			return err
		}
		return expect(n == 3 && ctr == 7, "rows %d counter %d", n, ctr)
	})

	check("lwt", func() error {
		m := map[string]any{}
		applied, err := s.Query("INSERT INTO lwt (k, v, n) VALUES (?, ?, ?) IF NOT EXISTS", 1, "first", 1).MapScanCAS(m)
		if err != nil || !applied {
			return fmt.Errorf("first insert: applied=%v err=%v", applied, err)
		}
		m = map[string]any{}
		applied, err = s.Query("INSERT INTO lwt (k, v, n) VALUES (?, ?, ?) IF NOT EXISTS", 1, "second", 2).MapScanCAS(m)
		if err != nil || applied || m["v"] != "first" {
			return fmt.Errorf("second insert: applied=%v existing=%v err=%v", applied, m, err)
		}
		// ScyllaDB returns the condition's columns even when applied; Cassandra only then not.
		if applied, err = s.Query("UPDATE lwt SET v = ? WHERE k = ? IF n = ?", "third", 1, 1).MapScanCAS(map[string]any{}); err != nil || !applied {
			return fmt.Errorf("conditional update: %v %v", applied, err)
		}
		m = map[string]any{}
		if applied, err = s.Query("UPDATE lwt SET v = ? WHERE k = ? IF n = ?", "fourth", 1, 5).MapScanCAS(m); err != nil || applied || m["n"] != 1 {
			return fmt.Errorf("failed condition: %v %v %v", applied, m, err)
		}
		return checkLWTMarking(s)
	})

	for i, t := range types {
		i, t := i, t
		check("types."+t.label, func() error {
			if err := typeSupported(t.label); err != nil {
				return err
			}
			col := "c_" + t.label
			// gocql can't bind a tuple marker (it counts the elements as markers but marshals the
			// column as a whole), so the tuple goes in as a literal and comes back flattened.
			q := s.Query(fmt.Sprintf("INSERT INTO types (k, %s) VALUES (?, ?)", col), i, t.value)
			if t.label == "tuple" {
				q = s.Query("INSERT INTO types (k, c_tuple) VALUES (?, (1, 't', 2.5))", i)
			}
			if err := q.Exec(); err != nil {
				return err
			}
			if t.label == "tuple" {
				var a int
				var b string
				var c float64
				if err := s.Query("SELECT c_tuple FROM types WHERE k = ?", i).Scan(&a, &b, &c); err != nil {
					return err
				}
				return expect(a == 1 && b == "t" && c == 2.5, "(%d, %s, %v)", a, b, c)
			}
			dst := t.dest()
			if err := s.Query(fmt.Sprintf("SELECT %s FROM types WHERE k = ?", col), i).Scan(dst); err != nil {
				return err
			}
			got := reflect.ValueOf(dst).Elem().Interface()
			want := t.value
			switch w := want.(type) {
			case *inf.Dec:
				return expect(got.(*inf.Dec).Cmp(w) == 0 && got.(*inf.Dec).Scale() == w.Scale(), "%v != %v", got, w)
			case *big.Int:
				return expect(got.(*big.Int).Cmp(w) == 0, "%v != %v", got, w)
			case net.IP:
				return expect(got.(net.IP).Equal(w), "%v != %v", got, w)
			case time.Time:
				return expect(got.(time.Time).Equal(w), "%v != %v", got, w)
			case []string:
				g := append([]string(nil), got.([]string)...)
				sort.Strings(g)
				return expect(reflect.DeepEqual(g, w), "%v != %v", g, w)
			case addr:
				g := got.(addr)
				sort.Strings(g.Tags)
				return expect(reflect.DeepEqual(g, w), "%+v != %+v", g, w)
			}
			return expect(reflect.DeepEqual(got, want), "%#v != %#v", got, want)
		})
	}

	check("token_aware", func() error {
		for _, k := range []int{1, 42, -7, 123456} {
			if err := s.Query("INSERT INTO kv (k, c, v) VALUES (?, 0, 't')", k).Exec(); err != nil {
				return err
			}
			q := s.Query("SELECT token(k) FROM kv WHERE k = ?", k)
			rk, err := q.GetRoutingKey()
			if err != nil {
				return err
			}
			iter := q.Iter()
			var server int64
			iter.Scan(&server)
			h := iter.Host()
			if err := iter.Close(); err != nil {
				return err
			}
			if err := expect(murmur3(rk) == server, "k=%d: driver %d server %d", k, murmur3(rk), server); err != nil {
				return err
			}
			if err := expect(h != nil, "no host"); err != nil {
				return err
			}
		}
		return nil
	})

	check("shard_aware", func() error { return checkShards(s) })

	check("tracing", func() error {
		var buf bytes.Buffer
		if err := s.Query("SELECT * FROM kv WHERE k = 1").Trace(gocql.NewTraceWriter(s, &buf)).Exec(); err != nil {
			return err
		}
		t := buf.String()
		return expect(strings.Contains(t, "Tracing session") && strings.Count(t, "\n") > 2, "trace %q", t)
	})

	check("events.schema", func() error { return checkEvents(s) })

	check("reconnect", func() error {
		if !restart {
			return skip("the harness doesn't restart this server")
		}
		var v string
		if err := s.Query("SELECT v FROM kv WHERE k = ? AND c = ?", 1, 1).Scan(&v); err != nil || v != "v1" {
			return fmt.Errorf("before restart: %q %v", v, err)
		}
		out("@@RESTART")
		bufio.NewReader(os.Stdin).ReadString('\n')
		var last error
		err := eventually(60*time.Second, func() bool {
			last = s.Query("SELECT v FROM kv WHERE k = ? AND c = ?", 1, 1).Scan(&v)
			return last == nil && v == "v1"
		})
		if err != nil {
			return fmt.Errorf("after restart: %v (last %v)", err, last)
		}
		return nil
	})

	s.Query("DROP KEYSPACE IF EXISTS " + ks).Exec()
	s.Close()
	out(fmt.Sprintf("DONE %d failed", failed))
	if failed > 0 {
		os.Exit(1)
	}
}
