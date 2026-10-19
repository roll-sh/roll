# frozen_string_literal: true

# roll queues compatibility scenarios for Ruby Bunny.

require "bunny"
require "digest"
require "securerandom"
require "timeout"

URL = ENV.fetch("AMQP_URL", "amqp://guest:guest@localhost:5672/%2F")
P = "bunny-#{SecureRandom.hex(3)}"
SCENARIOS = []
OPEN = []

def scenario(name, &blk)
  SCENARIOS << [name, blk]
end

def check(cond, msg)
  raise "assertion failed: #{msg}" unless cond
end

def connect(**opts)
  c = Bunny.new(URL, automatically_recover: false, log_level: :fatal, **opts)
  c.start
  OPEN << c
  c
end

def count(ch, q)
  ch.queue_declare(q, passive: true).message_count
end

# basic.get returning [Basic::GetOk, MessageProperties, body] or [nil, nil, nil]
# (raw get-ok because Bunny::GetResponse does not expose message_count)
def get(ch, q, manual_ack: true)
  ok, props, body = ch.basic_get(q, manual_ack: manual_ack)
  return [nil, nil, nil] unless props

  [ok, Bunny::MessageProperties.new(props), body]
end

def get_wait(ch, q, timeout = 5)
  deadline = Time.now + timeout
  while Time.now < deadline
    r = get(ch, q)
    return r if r[0]

    sleep 0.05
  end
  raise "no message on #{q} within #{timeout}s"
end

def expect_close(code)
  yield
rescue Bunny::ChannelLevelException => e
  got = e.channel_close.reply_code
  check(got == code, "expected #{code}, got #{got} #{e.channel_close.reply_text}")
  e.channel_close.reply_text
else
  raise "expected channel error #{code}"
end

def wait_for(timeout, what)
  deadline = Time.now + timeout
  until yield
    raise "timed out waiting for #{what}" if Time.now > deadline

    sleep 0.02
  end
end

scenario "connect" do
  c = connect
  props = c.server_properties
  check(c.open?, "connection not open")
  ch = c.create_channel
  check(ch.open?, "channel not open")
  c.close
  "#{props['product']} #{props['version']}"
end

scenario "declare" do
  c = connect
  ch = c.create_channel
  x = "#{P}.declare.x"
  q = "#{P}.declare.q"
  ch.exchange_declare(x, "direct", durable: true)
  ok = ch.queue_declare(q, durable: true)
  check(ok.queue == q && ok.message_count.zero?, "declare-ok #{ok.inspect}")
  ch.queue_bind(q, x, routing_key: "k")
  ch.exchange_declare(x, "direct", passive: true)
  srv = ch.queue_declare("", exclusive: true).queue
  check(srv.start_with?("amq.gen-"), "server-named queue #{srv}")
  text = expect_close(406) { ch.queue_declare(q, durable: false) }
  check(text.include?("inequivalent arg 'durable'"), text)
  ch = c.create_channel
  expect_close(404) { ch.queue_declare("#{P}.missing", passive: true) }
  ch = c.create_channel
  ch.queue_unbind(q, x, routing_key: "k")
  ch.queue_delete(q)
  ch.exchange_delete(x)
  c.close
  nil
end

scenario "publish_confirms" do
  c = connect
  ch = c.create_channel
  q = "#{P}.confirms"
  ch.queue_declare(q, durable: true)
  ch.confirm_select
  returned = []
  ch.default_exchange.on_return { |info, _props, _body| returned << info.routing_key }
  100.times { |i| ch.default_exchange.publish("m#{i}", routing_key: q, persistent: true, mandatory: true) }
  check(ch.wait_for_confirms, "nacked: #{ch.nacked_set.to_a}")
  check(returned.empty?, "returned #{returned}")
  check(count(ch, q) == 100, "count #{count(ch, q)}")
  ch.default_exchange.publish("x", routing_key: "#{P}.nowhere", mandatory: true)
  check(ch.wait_for_confirms, "unroutable publish nacked")
  check(returned == ["#{P}.nowhere"], "returns before ack: #{returned}")
  ch.queue_delete(q)
  c.close
  "100 acked, mandatory return ok"
end

scenario "consume_ack" do
  c = connect
  ch = c.create_channel
  q = "#{P}.consume"
  ch.queue_declare(q)
  ch.confirm_select
  10.times { |i| ch.default_exchange.publish("m#{i}", routing_key: q) }
  ch.wait_for_confirms
  got = Queue.new
  consumer = ch.basic_consume(q, ch.generate_consumer_tag, false) do |di, _props, body|
    got << body
    ch.ack(di.delivery_tag)
  end
  bodies = Array.new(10) { got.pop(timeout: 5) || raise("timed out after #{got.size}") }
  ch.basic_cancel(consumer.consumer_tag)
  check(bodies == (0...10).map { |i| "m#{i}" }, "order #{bodies}")
  check(count(ch, q).zero?, "queue not empty after acks")
  ch.queue_delete(q)
  c.close
  nil
end

scenario "nack_requeue" do
  c = connect
  ch = c.create_channel
  q = "#{P}.nack"
  ch.queue_declare(q)
  ch.default_exchange.publish("again", routing_key: q)
  seen = Queue.new
  n = 0
  consumer = ch.basic_consume(q, ch.generate_consumer_tag, false) do |di, _props, body|
    n += 1
    seen << [di.redelivered?, body]
    n == 1 ? ch.nack(di.delivery_tag, false, true) : ch.ack(di.delivery_tag)
  end
  first = seen.pop(timeout: 5) || raise("timed out")
  second = seen.pop(timeout: 5) || raise("timed out waiting for redelivery")
  ch.basic_cancel(consumer.consumer_tag)
  check(!first[0], "first delivery flagged redelivered")
  check(second[0] && second[1] == "again", "requeued delivery not redelivered")
  ch.queue_delete(q)
  c.close
  nil
end

scenario "reject_dlx" do
  c = connect
  ch = c.create_channel
  dlx = "#{P}.dlx"
  dlq = "#{P}.dlq"
  q = "#{P}.work"
  ch.exchange_declare(dlx, "fanout")
  ch.queue_declare(dlq)
  ch.queue_bind(dlq, dlx)
  ch.queue_declare(q, arguments: { "x-dead-letter-exchange" => dlx })
  ch.default_exchange.publish("bad", routing_key: q)
  di, = get_wait(ch, q)
  ch.reject(di.delivery_tag, false)
  _, props, body = get_wait(ch, dlq)
  death = props.headers["x-death"][0]
  check(body == "bad", "body")
  check(death["reason"] == "rejected" && death["queue"] == q && death["count"] == 1, "x-death #{death}")
  [q, dlq].each { |n| ch.queue_delete(n) }
  ch.exchange_delete(dlx)
  c.close
  nil
end

scenario "topic" do
  c = connect
  ch = c.create_channel
  q1 = "#{P}.t1"
  q2 = "#{P}.t2"
  x = ch.topic("#{P}.topic")
  [[q1, "a.*"], [q2, "#.z"]].each do |q, k|
    ch.queue_declare(q)
    ch.queue_bind(q, x.name, routing_key: k)
  end
  ch.confirm_select
  returned = []
  x.on_return { |info, _props, _body| returned << info.routing_key }
  %w[a.b a.b.z x.z a.z b.c].each { |k| x.publish(k, routing_key: k, mandatory: true) }
  ch.wait_for_confirms
  check(returned == ["b.c"], "unroutable #{returned}")
  counts = [count(ch, q1), count(ch, q2)]
  check(counts == [2, 3], "counts #{counts}")
  [q1, q2].each { |q| ch.queue_delete(q) }
  x.delete
  c.close
  nil
end

scenario "headers" do
  c = connect
  ch = c.create_channel
  qa = "#{P}.hall"
  qb = "#{P}.hany"
  x = ch.headers("#{P}.headers")
  ch.queue_declare(qa)
  ch.queue_declare(qb)
  ch.queue_bind(qa, x.name, arguments: { "x-match" => "all", "format" => "pdf", "type" => "report" })
  ch.queue_bind(qb, x.name, arguments: { "x-match" => "any", "format" => "zip", "type" => "log" })
  ch.confirm_select
  returned = []
  x.on_return { |_info, props, _body| returned << props[:headers] }
  hs = [{ "format" => "pdf", "type" => "report" }, { "format" => "pdf" }, { "type" => "log" }, { "format" => "zip", "type" => "x" }]
  hs.each { |h| x.publish("h", headers: h, mandatory: true) }
  ch.wait_for_confirms
  check(returned == [{ "format" => "pdf" }], "unroutable #{returned}")
  counts = [count(ch, qa), count(ch, qb)]
  check(counts == [1, 2], "counts #{counts}")
  [qa, qb].each { |q| ch.queue_delete(q) }
  x.delete
  c.close
  nil
end

scenario "tx" do
  c = connect
  ch = c.create_channel
  q = "#{P}.tx"
  ch.queue_declare(q)
  ch.tx_select
  ch.default_exchange.publish("rolled back", routing_key: q)
  ch.tx_rollback
  check(count(ch, q).zero?, "rollback left a message")
  ch.default_exchange.publish("committed", routing_key: q)
  ch.default_exchange.publish("committed", routing_key: q)
  ch.tx_commit
  check(count(ch, q) == 2, "commit count #{count(ch, q)}")
  di, = get(ch, q)
  ch.ack(di.delivery_tag)
  ch.tx_rollback
  ch.close # the rolled-back ack leaves the message unacked, so closing requeues it
  ch = c.create_channel
  check(count(ch, q) == 2, "ack was not rolled back")
  ch.tx_select
  di, = get(ch, q)
  ch.ack(di.delivery_tag)
  ch.tx_commit
  ch.close
  ch = c.create_channel
  check(count(ch, q) == 1, "ack was not committed")
  ch.queue_delete(q)
  c.close
  nil
end

scenario "prefetch" do
  c = connect
  ch = c.create_channel
  q = "#{P}.prefetch"
  ch.queue_declare(q)
  ch.confirm_select
  20.times { |i| ch.default_exchange.publish(i.to_s, routing_key: q) }
  ch.wait_for_confirms
  ch.basic_qos(5)
  tags = Queue.new
  ch.basic_consume(q, ch.generate_consumer_tag, false) { |di, _props, _body| tags << di.delivery_tag }
  sleep 1
  check(tags.size == 5, "received #{tags.size} with prefetch 5")
  last = Array.new(5) { tags.pop }.last
  ch.ack(last, true)
  sleep 1
  check(tags.size == 5, "received #{tags.size + 5} after acking 5")
  ch.close
  ch = c.create_channel
  ch.queue_delete(q)
  c.close
  nil
end

scenario "ttl" do
  c = connect
  ch = c.create_channel
  q1 = "#{P}.ttlq"
  q2 = "#{P}.ttlm"
  ch.queue_declare(q1, arguments: { "x-message-ttl" => 200 })
  ch.queue_declare(q2)
  ch.confirm_select
  ch.default_exchange.publish("x", routing_key: q1)
  ch.default_exchange.publish("short", routing_key: q2, expiration: "200")
  ch.default_exchange.publish("long", routing_key: q2, expiration: "60000")
  ch.wait_for_confirms
  sleep 0.7
  check(get(ch, q1, manual_ack: false)[0].nil?, "queue TTL did not expire")
  di, _, body = get(ch, q2, manual_ack: false)
  check(di && body == "long", "per-message TTL: got #{body.inspect}")
  [q1, q2].each { |q| ch.queue_delete(q) }
  c.close
  nil
end

scenario "priority" do
  c = connect
  ch = c.create_channel
  q = "#{P}.prio"
  ch.queue_declare(q, arguments: { "x-max-priority" => 10 })
  ch.confirm_select
  [1, 9, 5, 0, 7].each { |p| ch.default_exchange.publish(p.to_s, routing_key: q, priority: p) }
  ch.wait_for_confirms
  got = Array.new(5) { get(ch, q, manual_ack: false)[2].to_i }
  check(got == [9, 7, 5, 1, 0], "order #{got}")
  ch.queue_delete(q)
  c.close
  nil
end

scenario "exclusive_autodelete" do
  a = connect
  b = connect
  cha = a.create_channel
  q = cha.queue_declare("", exclusive: true).queue
  chb = b.create_channel
  expect_close(405) { chb.queue_declare(q, passive: true) }
  a.close
  chb = b.create_channel
  expect_close(404) { chb.queue_declare(q, passive: true) }
  chb = b.create_channel
  ad = "#{P}.autodel"
  chb.queue_declare(ad, auto_delete: true)
  consumer = chb.basic_consume(ad, chb.generate_consumer_tag, true) { |*| nil }
  chb.basic_cancel(consumer.consumer_tag)
  expect_close(404) { chb.queue_declare(ad, passive: true) }
  b.close
  nil
end

scenario "direct_reply_to" do
  c = connect
  client = c.create_channel
  server = c.create_channel
  q = "#{P}.rpc"
  server.queue_declare(q)
  server.basic_consume(q, server.generate_consumer_tag, false) do |di, props, body|
    server.default_exchange.publish(body.upcase, routing_key: props.reply_to, correlation_id: props.correlation_id)
    server.ack(di.delivery_tag)
  end
  replies = Queue.new
  client.basic_consume("amq.rabbitmq.reply-to", client.generate_consumer_tag, true) do |_di, props, body|
    replies << [props.correlation_id, body]
  end
  client.default_exchange.publish("ping", routing_key: q, reply_to: "amq.rabbitmq.reply-to", correlation_id: "c1")
  reply = replies.pop(timeout: 5)
  sleep 0.1
  check(reply == %w[c1 PING] && replies.empty?, "replies #{reply.inspect} +#{replies.size}")
  server.queue_delete(q)
  c.close
  nil
end

scenario "basic_get" do
  c = connect
  ch = c.create_channel
  q = "#{P}.get"
  ch.queue_declare(q)
  ch.confirm_select
  3.times { |i| ch.default_exchange.publish(i.to_s, routing_key: q) }
  ch.wait_for_confirms
  di, _, body = get(ch, q)
  check(di.message_count == 2 && body == "0", "get-ok message_count #{di.message_count}")
  ch.ack(di.delivery_tag)
  get(ch, q, manual_ack: false)
  get(ch, q, manual_ack: false)
  check(get(ch, q)[0].nil?, "expected get-empty")
  ch.queue_delete(q)
  c.close
  nil
end

scenario "cancel_notify" do
  c = connect
  ch = c.create_channel
  other = c.create_channel
  q = "#{P}.cancel"
  ch.queue_declare(q)
  cancelled = Queue.new
  consumer = Bunny::Consumer.new(ch, q, ch.generate_consumer_tag, false)
  consumer.on_delivery { |*| nil }
  consumer.on_cancellation { |frame| cancelled << frame.consumer_tag }
  ch.basic_consume_with(consumer)
  other.queue_delete(q)
  tag = cancelled.pop(timeout: 5)
  check(tag == consumer.consumer_tag, "cancel notification #{tag.inspect}")
  c.close
  nil
end

scenario "connection_blocked" do
  c = connect
  caps = c.server_capabilities
  check(caps["connection.blocked"] == true, "capabilities #{caps}")
  c.on_blocked { |*| nil }
  c.on_unblocked { nil }
  c.close
  nil
end

scenario "heartbeat" do
  c = connect(heartbeat: 2)
  check(c.heartbeat == 2, "negotiated heartbeat #{c.heartbeat}")
  sleep 7
  check(c.open?, "connection closed while idle")
  ch = c.create_channel
  ch.queue_declare("", exclusive: true)
  c.close
  "2s heartbeat, idle 7s"
end

scenario "large_message" do
  c = connect
  ch = c.create_channel
  q = "#{P}.large"
  ch.queue_declare(q)
  ch.confirm_select
  body = SecureRandom.random_bytes(5 * 1024 * 1024)
  ch.default_exchange.publish(body, routing_key: q)
  ch.wait_for_confirms
  _, _, got = get(ch, q, manual_ack: false)
  check(got && Digest::SHA256.digest(got) == Digest::SHA256.digest(body), "body mismatch")
  ch.queue_delete(q)
  c.close
  "5 MiB"
end

scenario "throughput" do
  n = 10_000
  c = connect
  ch = c.create_channel
  q = "#{P}.tput"
  ch.queue_declare(q)
  body = "x" * 100
  x = ch.default_exchange
  t0 = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  n.times { x.publish(body, routing_key: q) }
  pub = Process.clock_gettime(Process::CLOCK_MONOTONIC) - t0
  ch.basic_qos(500)
  done = Queue.new
  got = 0
  t1 = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  consumer = ch.basic_consume(q, ch.generate_consumer_tag, false) do |di, _props, _body|
    got += 1
    ch.ack(di.delivery_tag, true) if (got % 100).zero? || got == n
    done << true if got == n
  end
  done.pop(timeout: 10) || raise("timed out after #{got}")
  con = Process.clock_gettime(Process::CLOCK_MONOTONIC) - t1
  ch.basic_cancel(consumer.consumer_tag)
  ch.queue_delete(q)
  c.close
  format("publish %<p>.0f msg/s, consume %<c>.0f msg/s", p: n / pub, c: n / con)
end

def main
  only = ARGV.to_a
  failed = 0
  SCENARIOS.each do |name, blk|
    next unless only.empty? || only.include?(name)

    t = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    begin
      detail = Timeout.timeout(15) { blk.call }
      elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC) - t
      puts ["PASS", name, detail, format("(%.1fs)", elapsed)].compact.join(" ")
    rescue Exception => e # rubocop:disable Lint/RescueException
      raise if e.is_a?(Interrupt)

      failed += 1
      puts "FAIL #{name}: #{e.class}: #{e.message}"
      warn e.full_message
    ensure
      OPEN.each { |c| c.close if c.open? rescue nil }
      OPEN.clear
    end
    $stdout.flush
  end
  exit(failed.positive? ? 1 : 0)
end

main
