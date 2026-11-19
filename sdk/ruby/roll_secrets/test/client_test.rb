# frozen_string_literal: true

require "minitest/autorun"
require "roll_secrets"
require_relative "fake_server"

# The secret key whose bytes are 1..32, the key of both vectors below.
VECTOR_KEY = RollSecrets::Seal.private_key((1..32).to_a.pack("C*"))

# @roll/secrets' own vector (packages/secrets/test/seal.test.ts): sealed by rolld
# (crates/roll-secrets/src/crypto.rs seal_to, through POST …/resolve with seal_to).
FROM_RUST_CONTEXT = "roll-secrets:prj_fzuo337olof3:sdk-vector:app:web"
FROM_RUST = {
  "v" => 1,
  "epk" => "9PO9fuY7QZoMBtaL5PMjpi6H10M9S1AwI39B8G0vFwU",
  "nonce" => "ksbfrm7uIhUUg1Fd",
  "ct" => "ynCWA9hcEEbY57aJSraE_2EF7dVVVHEZcQKs45UeXnOWMVc3Fg6lLgCp2YO8kBSphzOUGSYDPA1XdwZl6A7UrOU"
}.freeze

# A box sealed by @roll/secrets' sealTo (the same box as the server's seal_to): the same vector as
# sdk/go/secrets and sdk/python/roll_secrets.
CONTEXT = "roll-secrets:prj_a:production:app:web"
BOX = {
  "v" => 1,
  "epk" => "aAGjVaESpknkSH_uG48K1a_unnp97Gh4uRwOYWrjHkk",
  "nonce" => "yct4_uAUdnUKxskR",
  "ct" => "-UbR7dzvRmoXgBHnbyoW00qyGY07tIB_0vx6VrXQhPPRNns7PU3irIJq5M5Xjto"
}.freeze

class SealedBoxesTest < Minitest::Test
  def test_a_box_sealed_by_roll_opens_only_for_its_context
    assert_equal "B6N8vBQgk8i3VdwbEOhstCY3StFqqFPtC9_AsrhtHHw", RollSecrets.public_key(VECTOR_KEY)
    assert_equal '{"DATABASE_URL":"postgres://app:hunter2@db/main"}', RollSecrets.open_sealed(FROM_RUST, VECTOR_KEY, FROM_RUST_CONTEXT)
    assert_equal '{"DB_URL":"postgres://db/shop"}', RollSecrets.open_sealed(BOX, VECTOR_KEY, CONTEXT)
    assert_raises(ArgumentError) { RollSecrets.open_sealed(FROM_RUST, VECTOR_KEY, "roll-secrets:prj_fzuo337olof3:sdk-vector:app:api") }
    assert_raises(ArgumentError) { RollSecrets.open_sealed(BOX, VECTOR_KEY, "roll-secrets:prj_a:staging:app:web") }
    assert_raises(ArgumentError) { RollSecrets.open_sealed(BOX, RollSecrets.generate_key, CONTEXT) }
  end
end

# Sets ENV for one test and puts it back.
module WithEnv
  def with_env(vars)
    before = vars.keys.to_h { |k| [k, ENV[k]] }
    vars.each { |k, v| ENV[k] = v }
    yield
  ensure
    before.each { |k, v| ENV[k] = v }
  end
end

class AWorkloadTest < Minitest::Test
  include WithEnv

  ENV_PATH = "/v1/secrets/projects/prj_a/environments/production"

  def test_logs_in_reads_sealed_values_and_watches_for_changes
    state = { digest: "d1", value: "one", token: "rlt_workload", logins: 0 }
    lock = Mutex.new
    server = FakeServer.new do |method, path, headers, body|
      lock.synchronize do
        if method == "POST" && path == "/v1/secrets/login"
          next FakeServer.json(401, { "error" => "no identity idn_ci takes this token", "code" => "unauthorized" }) unless [body["identity"], body["token"]] == ["idn_ci", "the-oidc-token"]
          state[:logins] += 1
          next FakeServer.json(200, { "token" => state[:token], "project" => "prj_a", "expires_at" => ((Time.now.to_f + 3600) * 1000).to_i })
        end
        next FakeServer.json(401) unless headers["authorization"] == "Bearer #{state[:token]}"
        case [method, path]
        when ["POST", "#{ENV_PATH}/resolve"]
          next FakeServer.json(400) unless [body["consumer"], body["seal_to"]] == ["app:web", RollSecrets.public_key(VECTOR_KEY)]
          FakeServer.json(200, { "sealed" => BOX, "context" => CONTEXT, "problems" => [], "leases" => [], "dynamic" => [], "linked" => ["DB_URL"] })
        when ["GET", "#{ENV_PATH}/digest"]
          FakeServer.json(200, { "digest" => state[:digest] })
        when ["GET", "#{ENV_PATH}/values"]
          FakeServer.json(200, { "values" => { "A" => state[:value] }, "problems" => [], "leases" => [] })
        else
          FakeServer.json(404)
        end
      end
    end

    with_env("ROLL_API_URL" => server.url, "ROLL_IDENTITY" => "idn_ci", "ROLL_OIDC_TOKEN" => "wrong", "ROLL_TOKEN" => nil, "ROLL_PROJECT" => nil) do
      e = assert_raises(RollSecrets::SecretsError) { RollSecrets.from_env }
      assert_equal [401, "unauthorized", "no identity idn_ci takes this token"], [e.status, e.code, e.message]
    end

    # Kept for the whole test: a new login reads the OIDC token again.
    with_env("ROLL_API_URL" => server.url, "ROLL_IDENTITY" => "idn_ci", "ROLL_OIDC_TOKEN" => "the-oidc-token", "ROLL_TOKEN" => nil, "ROLL_PROJECT" => nil) do
      c = RollSecrets.from_env
      refute_includes c.inspect, "rlt_workload", "a client's inspect never shows its token"

      sealed = c.resolve_sealed("production", "app:web", VECTOR_KEY)
      assert_equal [{ "DB_URL" => "postgres://db/shop" }, ["DB_URL"]], [sealed.values, sealed.linked]
      refute_includes sealed.inspect, "postgres", "inspect names keys, never values"

      seen = Queue.new
      stop = RollSecrets::Stop.new
      watching = Thread.new { c.watch("production", every: 0.01, stop: stop) { |v| seen << v.values["A"] } }
      begin
        assert_equal "one", pop(seen)
        lock.synchronize { state.update(digest: "d2", value: "two") }
        assert_equal "two", pop(seen)
        sleep 0.05
        assert seen.empty?, "an unchanged digest delivers nothing again"
        # The roll token stops working (revoked, ended): the client logs in again and keeps watching.
        lock.synchronize { state.update(digest: "d3", value: "three", token: "rlt_workload2") }
        assert_equal "three", pop(seen)
        assert_equal 2, lock.synchronize { state[:logins] }
      ensure
        stop.set
        watching.join
      end
    end
  ensure
    server&.close
  end

  private

  def pop(queue)
    deadline = Time.now + 5
    while Time.now < deadline
      return queue.pop(true) unless queue.empty?
      sleep 0.005
    end
    flunk "timed out"
  end
end

class ACacheTest < Minitest::Test
  def test_values_are_kept_revalidated_and_never_kept_with_dynamic_credentials
    state = { digest: "d1", value: "1", dynamic: [] }
    asked = []
    server = FakeServer.new do |_method, path, headers, _body|
      tag = headers["if-none-match"]
      asked << tag
      next FakeServer.json(400) unless path.include?("digest=1")
      digest = state[:digest]
      next [304, { "ETag" => "\"#{digest}\"" }, ""] if tag == "\"#{digest}\""
      FakeServer.json(200, { "values" => { "A" => state[:value] }, "problems" => [], "leases" => [], "dynamic" => state[:dynamic], "linked" => [] }, "ETag" => "\"#{digest}\"")
    end
    clock = 0.0
    c = RollSecrets::Client.new(server.url, "rlt_test", project: "prj_1", cache_ttl: 20, clock: -> { clock })

    assert_equal ["1", "1", 1], [c.values("production").values["A"], c.values("production").values["A"], asked.size]
    clock += 25
    assert_equal "1", c.values("production").values["A"]
    assert_equal ['"d1"'], asked[1..], "revalidated: a 304 keeps them"
    clock += 25
    state.update(digest: "d2", value: "2", dynamic: ["PW"])
    assert_equal ["2", "2", 4], [c.values("production").values["A"], c.values("production").values["A"], asked.size]
  ensure
    server&.close
  end
end

class ATransitKeyTest < Minitest::Test
  def test_sends_base64_and_decrypts_back_to_the_same_bytes
    sent = []
    # A transit endpoint that "encrypts" by echoing: enough to check the encoding both ways.
    server = FakeServer.new do |_method, path, _headers, body|
      sent << { "path" => path }.merge(body)
      out = path.end_with?("/encrypt") ? { "ciphertext" => "roll:v1:#{body["plaintext"]}" } : { "plaintext" => body["ciphertext"].delete_prefix("roll:v1:") }
      FakeServer.json(200, out)
    end
    cards = RollSecrets::Client.new(server.url, "rlt_test", project: "prj_1").transit("cards")

    ct = cards.encrypt("héllo ✓", "order:42")
    assert_equal({ "path" => "/v1/secrets/projects/prj_1/transit/cards/encrypt", "plaintext" => "aMOpbGxvIOKckw==", "context" => "order:42" }, sent[0])
    assert_equal "héllo ✓", cards.decrypt(ct, "order:42").force_encoding(Encoding::UTF_8)
    raw = [0, 255, 128, 7].pack("C*")
    assert_equal raw, cards.decrypt(cards.encrypt(raw))
  ensure
    server&.close
  end
end
