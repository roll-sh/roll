// roll queues compatibility scenarios for Go github.com/rabbitmq/amqp091-go.
package main

import (
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"reflect"
	"runtime/debug"
	"strings"
	"sync"
	"time"

	amqp "github.com/rabbitmq/amqp091-go"
)

var (
	url = env("AMQP_URL", "amqp://guest:guest@localhost:5672/%2F")
	P   = "go-" + randHex(3)
)

func env(k, def string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return def
}

func randHex(n int) string {
	b := make([]byte, n)
	rand.Read(b)
	return hex.EncodeToString(b)
}

// failure is raised (via panic) by check/must and turned into a FAIL line.
type failure struct{ msg string }

func check(cond bool, format string, args ...any) {
	if !cond {
		panic(failure{fmt.Sprintf(format, args...)})
	}
}

func must(err error) {
	if err != nil {
		panic(failure{fmt.Sprintf("%T: %v", err, err)})
	}
}

func must1[T any](v T, err error) T {
	must(err)
	return v
}

// T tracks the connections a scenario opens so they can be closed on timeout.
type T struct {
	mu    sync.Mutex
	conns []*amqp.Connection
}

func (t *T) dial(cfg *amqp.Config) *amqp.Connection {
	c := amqp.Config{Heartbeat: 10 * time.Second}
	if cfg != nil {
		c = *cfg
	}
	c.Dial = amqp.DefaultDial(5 * time.Second)
	conn := must1(amqp.DialConfig(url, c))
	t.mu.Lock()
	t.conns = append(t.conns, conn)
	t.mu.Unlock()
	return conn
}

func (t *T) closeAll() {
	t.mu.Lock()
	defer t.mu.Unlock()
	for _, c := range t.conns {
		if !c.IsClosed() {
			c.CloseDeadline(time.Now().Add(2 * time.Second))
		}
	}
}

func channel(c *amqp.Connection) *amqp.Channel { return must1(c.Channel()) }

func declare(ch *amqp.Channel, q string, args amqp.Table) {
	must1(ch.QueueDeclare(q, false, false, false, false, args))
}

func count(ch *amqp.Channel, q string) int {
	return must1(ch.QueueDeclarePassive(q, false, false, false, false, nil)).Messages
}

func publish(ch *amqp.Channel, x, k string, body []byte) {
	must(ch.Publish(x, k, false, false, amqp.Publishing{Body: body}))
}

// confirmed publishes on a channel in confirm mode and waits for the broker ack.
func confirmed(ch *amqp.Channel, x, k string, mandatory bool, msg amqp.Publishing) {
	dc := must1(ch.PublishWithDeferredConfirm(x, k, mandatory, false, msg))
	waitAck(dc)
}

func waitAck(dc *amqp.DeferredConfirmation) {
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	ok, err := dc.WaitContext(ctx)
	must(err)
	check(ok, "publish %d nacked", dc.DeliveryTag)
}

func recv(ds <-chan amqp.Delivery, timeout time.Duration, what string) amqp.Delivery {
	select {
	case d, ok := <-ds:
		check(ok, "delivery channel closed waiting for %s", what)
		return d
	case <-time.After(timeout):
		panic(failure{fmt.Sprintf("timed out waiting for %s", what)})
	}
}

func getWait(ch *amqp.Channel, q string) amqp.Delivery {
	end := time.Now().Add(5 * time.Second)
	for time.Now().Before(end) {
		m, ok, err := ch.Get(q, false)
		must(err)
		if ok {
			return m
		}
		time.Sleep(50 * time.Millisecond)
	}
	panic(failure{fmt.Sprintf("no message on %s within 5s", q)})
}

// expectClose asserts that err is a channel/connection exception with the given code.
func expectClose(err error, code int) string {
	var e *amqp.Error
	check(errors.As(err, &e), "expected channel error %d, got %v", code, err)
	check(e.Code == code, "expected %d, got %d %s", code, e.Code, e.Reason)
	return e.Reason
}

func drainReturns(rs <-chan amqp.Return) []amqp.Return {
	var out []amqp.Return
	for {
		select {
		case r := <-rs:
			out = append(out, r)
		default:
			return out
		}
	}
}

func toInt(v any) int64 {
	switch n := v.(type) {
	case int:
		return int64(n)
	case int8:
		return int64(n)
	case int16:
		return int64(n)
	case int32:
		return int64(n)
	case int64:
		return n
	case uint8:
		return int64(n)
	case uint16:
		return int64(n)
	case uint32:
		return int64(n)
	case uint64:
		return int64(n)
	}
	return -1
}

// ---------------------------------------------------------------- scenarios

func sConnect(t *T) string {
	c := t.dial(nil)
	check(!c.IsClosed(), "connection not open")
	ch := channel(c)
	check(!ch.IsClosed(), "channel not open")
	must(c.Close())
	return fmt.Sprintf("%v %v", c.Properties["product"], c.Properties["version"])
}

func sDeclare(t *T) string {
	c := t.dial(nil)
	ch := channel(c)
	x, q := P+".declare.x", P+".declare.q"
	must(ch.ExchangeDeclare(x, "direct", true, false, false, false, nil))
	ok := must1(ch.QueueDeclare(q, true, false, false, false, nil))
	check(ok.Name == q && ok.Messages == 0, "declare-ok %+v", ok)
	must(ch.QueueBind(q, "k", x, false, nil))
	must(ch.ExchangeDeclarePassive(x, "direct", true, false, false, false, nil))
	srv := must1(ch.QueueDeclare("", false, false, true, false, nil)).Name
	check(strings.HasPrefix(srv, "amq.gen-"), "server-named queue %s", srv)
	_, err := ch.QueueDeclare(q, false, false, false, false, nil)
	text := expectClose(err, 406)
	check(strings.Contains(text, "inequivalent arg 'durable'"), "%s", text)
	ch = channel(c)
	_, err = ch.QueueDeclarePassive(P+".missing", false, false, false, false, nil)
	expectClose(err, 404)
	ch = channel(c)
	must(ch.QueueUnbind(q, "k", x, nil))
	must1(ch.QueueDelete(q, false, false, false))
	must(ch.ExchangeDelete(x, false, false))
	must(c.Close())
	return ""
}

func sPublishConfirms(t *T) string {
	c := t.dial(nil)
	ch := channel(c)
	q := P + ".confirms"
	must1(ch.QueueDeclare(q, true, false, false, false, nil))
	must(ch.Confirm(false))
	confirms := ch.NotifyPublish(make(chan amqp.Confirmation, 128))
	returns := ch.NotifyReturn(make(chan amqp.Return, 4))
	var dcs []*amqp.DeferredConfirmation
	for i := 0; i < 100; i++ {
		msg := amqp.Publishing{DeliveryMode: amqp.Persistent, Body: []byte(fmt.Sprintf("m%d", i))}
		dcs = append(dcs, must1(ch.PublishWithDeferredConfirm("", q, true, false, msg)))
	}
	for _, dc := range dcs {
		waitAck(dc)
	}
	for i := uint64(1); i <= 100; i++ {
		select {
		case cf := <-confirms:
			check(cf.Ack && cf.DeliveryTag == i, "confirmation %+v, want ack %d", cf, i)
		case <-time.After(5 * time.Second):
			panic(failure{fmt.Sprintf("NotifyPublish: missing confirmation %d", i)})
		}
	}
	check(count(ch, q) == 100, "count %d", count(ch, q))
	dc := must1(ch.PublishWithDeferredConfirm("", P+".nowhere", true, false, amqp.Publishing{Body: []byte("x")}))
	waitAck(dc)
	select {
	case r := <-returns:
		check(r.ReplyCode == 312 && r.RoutingKey == P+".nowhere", "return %d %s %s", r.ReplyCode, r.ReplyText, r.RoutingKey)
	case <-time.After(5 * time.Second):
		panic(failure{"unroutable mandatory publish was not returned"})
	}
	must1(ch.QueueDelete(q, false, false, false))
	must(c.Close())
	return "100 acked, mandatory return ok"
}

func sConsumeAck(t *T) string {
	c := t.dial(nil)
	ch := channel(c)
	q := P + ".consume"
	declare(ch, q, nil)
	must(ch.Confirm(false))
	for i := 0; i < 10; i++ {
		confirmed(ch, "", q, false, amqp.Publishing{Body: []byte(fmt.Sprintf("m%d", i))})
	}
	ds := must1(ch.Consume(q, "go-consume", false, false, false, false, nil))
	var got, want []string
	for i := 0; i < 10; i++ {
		d := recv(ds, 5*time.Second, fmt.Sprintf("message %d", i))
		got = append(got, string(d.Body))
		want = append(want, fmt.Sprintf("m%d", i))
		must(d.Ack(false))
	}
	must(ch.Cancel("go-consume", false))
	check(reflect.DeepEqual(got, want), "order %v", got)
	check(count(ch, q) == 0, "queue not empty after acks")
	must1(ch.QueueDelete(q, false, false, false))
	must(c.Close())
	return ""
}

func sNackRequeue(t *T) string {
	c := t.dial(nil)
	ch := channel(c)
	q := P + ".nack"
	declare(ch, q, nil)
	publish(ch, "", q, []byte("again"))
	ds := must1(ch.Consume(q, "go-nack", false, false, false, false, nil))
	d := recv(ds, 5*time.Second, "first delivery")
	check(!d.Redelivered, "first delivery flagged redelivered")
	must(d.Nack(false, true))
	d = recv(ds, 5*time.Second, "redelivery")
	check(d.Redelivered && string(d.Body) == "again", "requeued delivery not redelivered")
	must(d.Ack(false))
	must(ch.Cancel("go-nack", false))
	must1(ch.QueueDelete(q, false, false, false))
	must(c.Close())
	return ""
}

func sRejectDLX(t *T) string {
	c := t.dial(nil)
	ch := channel(c)
	dlx, dlq, q := P+".dlx", P+".dlq", P+".work"
	must(ch.ExchangeDeclare(dlx, "fanout", false, false, false, false, nil))
	declare(ch, dlq, nil)
	must(ch.QueueBind(dlq, "", dlx, false, nil))
	declare(ch, q, amqp.Table{"x-dead-letter-exchange": dlx})
	publish(ch, "", q, []byte("bad"))
	m := getWait(ch, q)
	must(m.Reject(false))
	d := getWait(ch, dlq)
	check(string(d.Body) == "bad", "body")
	deaths, ok := d.Headers["x-death"].([]any)
	check(ok && len(deaths) > 0, "x-death header %v", d.Headers)
	death, ok := deaths[0].(amqp.Table)
	check(ok, "x-death[0] %T", deaths[0])
	check(death["reason"] == "rejected" && death["queue"] == q && toInt(death["count"]) == 1, "x-death %v", death)
	must(d.Ack(false))
	for _, n := range []string{q, dlq} {
		must1(ch.QueueDelete(n, false, false, false))
	}
	must(ch.ExchangeDelete(dlx, false, false))
	must(c.Close())
	return ""
}

func sTopic(t *T) string {
	c := t.dial(nil)
	ch := channel(c)
	x, q1, q2 := P+".topic", P+".t1", P+".t2"
	must(ch.ExchangeDeclare(x, "topic", false, false, false, false, nil))
	for _, b := range [][2]string{{q1, "a.*"}, {q2, "#.z"}} {
		declare(ch, b[0], nil)
		must(ch.QueueBind(b[0], b[1], x, false, nil))
	}
	must(ch.Confirm(false))
	returns := ch.NotifyReturn(make(chan amqp.Return, 8))
	var returned []string
	for _, k := range []string{"a.b", "a.b.z", "x.z", "a.z", "b.c"} {
		confirmed(ch, x, k, true, amqp.Publishing{Body: []byte(k)})
		for _, r := range drainReturns(returns) {
			returned = append(returned, r.RoutingKey)
		}
	}
	check(reflect.DeepEqual(returned, []string{"b.c"}), "unroutable %v", returned)
	n1, n2 := count(ch, q1), count(ch, q2)
	check(n1 == 2 && n2 == 3, "counts %d,%d", n1, n2)
	for _, q := range []string{q1, q2} {
		must1(ch.QueueDelete(q, false, false, false))
	}
	must(ch.ExchangeDelete(x, false, false))
	must(c.Close())
	return ""
}

func sHeaders(t *T) string {
	c := t.dial(nil)
	ch := channel(c)
	x, qa, qb := P+".headers", P+".hall", P+".hany"
	must(ch.ExchangeDeclare(x, "headers", false, false, false, false, nil))
	declare(ch, qa, nil)
	declare(ch, qb, nil)
	must(ch.QueueBind(qa, "", x, false, amqp.Table{"x-match": "all", "format": "pdf", "type": "report"}))
	must(ch.QueueBind(qb, "", x, false, amqp.Table{"x-match": "any", "format": "zip", "type": "log"}))
	must(ch.Confirm(false))
	returns := ch.NotifyReturn(make(chan amqp.Return, 8))
	var returned []amqp.Table
	for _, h := range []amqp.Table{
		{"format": "pdf", "type": "report"}, {"format": "pdf"}, {"type": "log"}, {"format": "zip", "type": "x"},
	} {
		confirmed(ch, x, "", true, amqp.Publishing{Headers: h, Body: []byte("h")})
		for _, r := range drainReturns(returns) {
			returned = append(returned, r.Headers)
		}
	}
	check(reflect.DeepEqual(returned, []amqp.Table{{"format": "pdf"}}), "unroutable %v", returned)
	na, nb := count(ch, qa), count(ch, qb)
	check(na == 1 && nb == 2, "counts %d,%d", na, nb)
	for _, q := range []string{qa, qb} {
		must1(ch.QueueDelete(q, false, false, false))
	}
	must(ch.ExchangeDelete(x, false, false))
	must(c.Close())
	return ""
}

func sTx(t *T) string {
	c := t.dial(nil)
	ch := channel(c)
	q := P + ".tx"
	declare(ch, q, nil)
	must(ch.Tx())
	publish(ch, "", q, []byte("rolled back"))
	must(ch.TxRollback())
	check(count(ch, q) == 0, "rollback left a message")
	publish(ch, "", q, []byte("committed"))
	publish(ch, "", q, []byte("committed"))
	must(ch.TxCommit())
	check(count(ch, q) == 2, "commit count %d", count(ch, q))
	m, ok, err := ch.Get(q, false)
	must(err)
	check(ok, "get-empty after commit")
	must(m.Ack(false))
	must(ch.TxRollback())
	must(ch.Close()) // the rolled-back ack leaves the message unacked, so closing requeues it
	ch = channel(c)
	check(count(ch, q) == 2, "ack was not rolled back")
	must(ch.Tx())
	m, ok, err = ch.Get(q, false)
	must(err)
	check(ok, "get-empty in second tx")
	must(m.Ack(false))
	must(ch.TxCommit())
	must(ch.Close())
	ch = channel(c)
	check(count(ch, q) == 1, "ack was not committed")
	must1(ch.QueueDelete(q, false, false, false))
	must(c.Close())
	return ""
}

func sPrefetch(t *T) string {
	c := t.dial(nil)
	ch := channel(c)
	q := P + ".prefetch"
	declare(ch, q, nil)
	must(ch.Confirm(false))
	for i := 0; i < 20; i++ {
		confirmed(ch, "", q, false, amqp.Publishing{Body: []byte(fmt.Sprint(i))})
	}
	must(ch.Qos(5, 0, false))
	ds := must1(ch.Consume(q, "go-prefetch", false, false, false, false, nil))
	var tags []uint64
	collect := func(d time.Duration) {
		end := time.After(d)
		for {
			select {
			case m := <-ds:
				tags = append(tags, m.DeliveryTag)
			case <-end:
				return
			}
		}
	}
	collect(time.Second)
	check(len(tags) == 5, "received %d with prefetch 5", len(tags))
	must(ch.Ack(tags[len(tags)-1], true))
	collect(time.Second)
	check(len(tags) == 10, "received %d after acking 5", len(tags))
	must(ch.Close())
	ch = channel(c)
	must1(ch.QueueDelete(q, false, false, false))
	must(c.Close())
	return ""
}

func sTTL(t *T) string {
	c := t.dial(nil)
	ch := channel(c)
	q1, q2 := P+".ttlq", P+".ttlm"
	declare(ch, q1, amqp.Table{"x-message-ttl": int32(200)})
	declare(ch, q2, nil)
	must(ch.Confirm(false))
	confirmed(ch, "", q1, false, amqp.Publishing{Body: []byte("x")})
	confirmed(ch, "", q2, false, amqp.Publishing{Expiration: "200", Body: []byte("short")})
	confirmed(ch, "", q2, false, amqp.Publishing{Expiration: "60000", Body: []byte("long")})
	time.Sleep(700 * time.Millisecond)
	_, ok, err := ch.Get(q1, true)
	must(err)
	check(!ok, "queue TTL did not expire")
	m, ok, err := ch.Get(q2, true)
	must(err)
	check(ok && string(m.Body) == "long", "per-message TTL: got %q", m.Body)
	for _, q := range []string{q1, q2} {
		must1(ch.QueueDelete(q, false, false, false))
	}
	must(c.Close())
	return ""
}

func sPriority(t *T) string {
	c := t.dial(nil)
	ch := channel(c)
	q := P + ".prio"
	declare(ch, q, amqp.Table{"x-max-priority": int32(10)})
	must(ch.Confirm(false))
	for _, p := range []uint8{1, 9, 5, 0, 7} {
		confirmed(ch, "", q, false, amqp.Publishing{Priority: p, Body: []byte(fmt.Sprint(p))})
	}
	var got []string
	for i := 0; i < 5; i++ {
		m, ok, err := ch.Get(q, true)
		must(err)
		check(ok, "get-empty after %v", got)
		got = append(got, string(m.Body))
	}
	check(reflect.DeepEqual(got, []string{"9", "7", "5", "1", "0"}), "order %v", got)
	must1(ch.QueueDelete(q, false, false, false))
	must(c.Close())
	return ""
}

func sExclusiveAutodelete(t *T) string {
	a, b := t.dial(nil), t.dial(nil)
	cha := channel(a)
	q := must1(cha.QueueDeclare("", false, false, true, false, nil)).Name
	chb := channel(b)
	_, err := chb.QueueDeclarePassive(q, false, false, true, false, nil)
	expectClose(err, 405)
	must(a.Close())
	chb = channel(b)
	_, err = chb.QueueDeclarePassive(q, false, false, true, false, nil)
	expectClose(err, 404)
	chb = channel(b)
	ad := P + ".autodel"
	must1(chb.QueueDeclare(ad, false, true, false, false, nil))
	must1(chb.Consume(ad, "go-autodel", false, false, false, false, nil))
	must(chb.Cancel("go-autodel", false))
	_, err = chb.QueueDeclarePassive(ad, false, true, false, false, nil)
	expectClose(err, 404)
	must(b.Close())
	return ""
}

func sDirectReplyTo(t *T) string {
	c := t.dial(nil)
	client, server := channel(c), channel(c)
	q := P + ".rpc"
	declare(server, q, nil)
	reqs := must1(server.Consume(q, "go-rpc-server", false, false, false, false, nil))
	go func() {
		for d := range reqs {
			server.Publish("", d.ReplyTo, false, false, amqp.Publishing{
				CorrelationId: d.CorrelationId, Body: bytes.ToUpper(d.Body),
			})
			d.Ack(false)
		}
	}()
	replies := must1(client.Consume("amq.rabbitmq.reply-to", "go-rpc-client", true, false, false, false, nil))
	must(client.Publish("", q, false, false, amqp.Publishing{
		ReplyTo: "amq.rabbitmq.reply-to", CorrelationId: "c1", Body: []byte("ping"),
	}))
	r := recv(replies, 5*time.Second, "reply")
	check(r.CorrelationId == "c1" && string(r.Body) == "PING", "reply %q %q", r.CorrelationId, r.Body)
	must1(server.QueueDelete(q, false, false, false))
	must(c.Close())
	return ""
}

func sBasicGet(t *T) string {
	c := t.dial(nil)
	ch := channel(c)
	q := P + ".get"
	declare(ch, q, nil)
	must(ch.Confirm(false))
	for i := 0; i < 3; i++ {
		confirmed(ch, "", q, false, amqp.Publishing{Body: []byte(fmt.Sprint(i))})
	}
	m, ok, err := ch.Get(q, false)
	must(err)
	check(ok && m.MessageCount == 2 && string(m.Body) == "0", "get-ok message_count %d body %q", m.MessageCount, m.Body)
	must(m.Ack(false))
	for i := 0; i < 2; i++ {
		_, ok, err = ch.Get(q, true)
		must(err)
		check(ok, "unexpected get-empty")
	}
	_, ok, err = ch.Get(q, false)
	must(err)
	check(!ok, "expected get-empty")
	must1(ch.QueueDelete(q, false, false, false))
	must(c.Close())
	return ""
}

func sCancelNotify(t *T) string {
	c := t.dial(nil)
	ch, other := channel(c), channel(c)
	q := P + ".cancel"
	declare(ch, q, nil)
	cancels := ch.NotifyCancel(make(chan string, 1))
	tag := "go-cancel-" + randHex(3)
	ds := must1(ch.Consume(q, tag, false, false, false, false, nil))
	must1(other.QueueDelete(q, false, false, false))
	select {
	case got := <-cancels:
		check(got == tag, "cancel notification %q, want %q", got, tag)
	case <-time.After(5 * time.Second):
		panic(failure{"no basic.cancel notification"})
	}
	select {
	case _, open := <-ds:
		check(!open, "unexpected delivery after cancel")
	case <-time.After(2 * time.Second):
		panic(failure{"delivery channel not closed after cancel"})
	}
	must(c.Close())
	return ""
}

func sConnectionBlocked(t *T) string {
	c := t.dial(nil)
	caps, ok := c.Properties["capabilities"].(amqp.Table)
	check(ok && caps["connection.blocked"] == true, "capabilities %v", c.Properties["capabilities"])
	c.NotifyBlocked(make(chan amqp.Blocking, 1))
	must(c.Close())
	return ""
}

func sHeartbeat(t *T) string {
	c := t.dial(&amqp.Config{Heartbeat: 2 * time.Second})
	check(c.Config.Heartbeat == 2*time.Second, "negotiated heartbeat %v", c.Config.Heartbeat)
	closed := c.NotifyClose(make(chan *amqp.Error, 1))
	select {
	case e := <-closed:
		panic(failure{fmt.Sprintf("connection closed while idle: %v", e)})
	case <-time.After(7 * time.Second):
	}
	check(!c.IsClosed(), "connection closed while idle")
	ch := channel(c)
	must1(ch.QueueDeclare("", false, false, true, false, nil))
	must(c.Close())
	return "2s heartbeat, idle 7s"
}

func sLargeMessage(t *T) string {
	c := t.dial(nil)
	ch := channel(c)
	q := P + ".large"
	declare(ch, q, nil)
	must(ch.Confirm(false))
	body := make([]byte, 5*1024*1024)
	rand.Read(body)
	confirmed(ch, "", q, false, amqp.Publishing{Body: body})
	m, ok, err := ch.Get(q, true)
	must(err)
	check(ok, "get-empty")
	check(sha256.Sum256(m.Body) == sha256.Sum256(body), "body mismatch (%d bytes)", len(m.Body))
	must1(ch.QueueDelete(q, false, false, false))
	must(c.Close())
	return "5 MiB"
}

func sThroughput(t *T) string {
	const n = 10_000
	c := t.dial(nil)
	ch := channel(c)
	q := P + ".tput"
	declare(ch, q, nil)
	body := bytes.Repeat([]byte("x"), 100)
	t0 := time.Now()
	for i := 0; i < n; i++ {
		publish(ch, "", q, body)
	}
	pub := time.Since(t0)
	must(ch.Qos(500, 0, false))
	t1 := time.Now()
	ds := must1(ch.Consume(q, "go-tput", false, false, false, false, nil))
	for got := 1; got <= n; got++ {
		m := recv(ds, 10*time.Second, fmt.Sprintf("message %d", got))
		if got%100 == 0 || got == n {
			must(ch.Ack(m.DeliveryTag, true))
		}
	}
	con := time.Since(t1)
	must(ch.Cancel("go-tput", false))
	must1(ch.QueueDelete(q, false, false, false))
	must(c.Close())
	return fmt.Sprintf("publish %.0f msg/s, consume %.0f msg/s", n/pub.Seconds(), n/con.Seconds())
}

// ---------------------------------------------------------------- runner

type scenario struct {
	name    string
	fn      func(*T) string
	timeout time.Duration
}

var scenarios = []scenario{
	{"connect", sConnect, 0},
	{"declare", sDeclare, 0},
	{"publish_confirms", sPublishConfirms, 0},
	{"consume_ack", sConsumeAck, 0},
	{"nack_requeue", sNackRequeue, 0},
	{"reject_dlx", sRejectDLX, 0},
	{"topic", sTopic, 0},
	{"headers", sHeaders, 0},
	{"tx", sTx, 0},
	{"prefetch", sPrefetch, 0},
	{"ttl", sTTL, 0},
	{"priority", sPriority, 0},
	{"exclusive_autodelete", sExclusiveAutodelete, 0},
	{"direct_reply_to", sDirectReplyTo, 0},
	{"basic_get", sBasicGet, 0},
	{"cancel_notify", sCancelNotify, 0},
	{"connection_blocked", sConnectionBlocked, 0},
	{"heartbeat", sHeartbeat, 20 * time.Second},
	{"large_message", sLargeMessage, 0},
	{"throughput", sThroughput, 30 * time.Second},
}

func run(s scenario) (detail string, err error) {
	t := &T{}
	done := make(chan struct{})
	go func() {
		defer close(done)
		defer func() {
			if r := recover(); r != nil {
				if f, ok := r.(failure); ok {
					err = errors.New(f.msg)
				} else {
					err = fmt.Errorf("panic: %v", r)
				}
				fmt.Fprintf(os.Stderr, "--- %s: %v\n%s\n", s.name, r, debug.Stack())
			}
		}()
		detail = s.fn(t)
	}()
	timeout := s.timeout
	if timeout == 0 {
		timeout = 15 * time.Second
	}
	select {
	case <-done:
	case <-time.After(timeout):
		t.closeAll()
		return "", fmt.Errorf("timed out after %v", timeout)
	}
	t.closeAll()
	return detail, err
}

func main() {
	only := map[string]bool{}
	for _, a := range os.Args[1:] {
		only[a] = true
	}
	failed := 0
	for _, s := range scenarios {
		if len(only) > 0 && !only[s.name] {
			continue
		}
		start := time.Now()
		detail, err := run(s)
		if err != nil {
			failed++
			fmt.Printf("FAIL %s: %v\n", s.name, err)
			continue
		}
		line := fmt.Sprintf("PASS %s %s (%.1fs)", s.name, detail, time.Since(start).Seconds())
		fmt.Println(strings.ReplaceAll(line, "  ", " "))
	}
	if failed > 0 {
		os.Exit(1)
	}
}
