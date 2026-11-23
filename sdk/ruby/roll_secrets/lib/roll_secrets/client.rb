# frozen_string_literal: true

require "json"
require "net/http"
require "uri"

module RollSecrets
  # Reads one project's secrets.
  class Client
    ANSWER_LIMIT = 16 << 20
    ERROR_LIMIT = 1 << 16

    attr_reader :api, :timeout, :cache_ttl
    attr_accessor :token, :project
    # Renews the token of a client a workload identity's login made.
    attr_accessor :relogin # :nodoc:

    # +cache_ttl+ (seconds) keeps +values+ answers that long and answers from memory meanwhile;
    # after that it asks again with the answer's digest and every key's stamp (+…/values/delta+): an
    # unchanged folder costs what a 304 does, and a changed one only its changed keys. A folder
    # holding dynamic credentials is never kept (they end with their lease). +clock+ is for tests.
    def initialize(api, token, project: nil, timeout: 30.0, cache_ttl: 0.0, clock: -> { Process.clock_gettime(Process::CLOCK_MONOTONIC) })
      @api = api.to_s.sub(%r{/+\z}, "")
      @token = token
      @project = project
      @timeout = timeout
      @cache_ttl = cache_ttl
      @clock = clock
      @cache = {}
      @cache_lock = Mutex.new
      # A rolld without +…/values/delta+: reads are full (conditional, with cache_ttl).
      @no_delta = false
      @project_lock = Mutex.new
      @relogin = nil
    end

    # Names the API and the project, never the token.
    def inspect
      "#<#{self.class.name} api=#{@api.inspect} project=#{@project.inspect}>"
    end
    alias to_s inspect

    # The project: as given, or the token's own (GET /v1/secrets/whoami).
    def project_id
      @project_lock.synchronize do
        unless @project && !@project.empty?
          who = call("GET", "/v1/secrets/whoami") || {}
          raise SecretsError.new("this token isn't limited to one project: pass project", 400, "project") if who["project"].to_s.empty?
          @project = who["project"]
        end
        @project
      end
    end

    # +env+'s values under +path+ (the root by default), references resolved. +fresh+ skips
    # +cache_ttl+ (a watch that saw the digest move reads anew).
    def values(env, path: "", fresh: false)
      e = env_path(env)
      url = e + "/values" + query(path)
      if @cache_ttl <= 0 || fresh
        v = Values.of(call("GET", url))
        @cache_lock.synchronize { @cache.delete(url) } if fresh
        return v
      end
      Marshal.load(Marshal.dump(cached(e, query(path))))
    end

    # +env+'s values as +consumer+ (+app:web+), what a running service gets: dynamic credentials'
    # leases are the consumer's, and the read is the consumer's in the audit log.
    def resolve(env, consumer, path: "")
      Values.of(call("POST", env_path(env) + "/resolve", { "consumer" => consumer, "path" => or_root(path) }))
    end

    # +resolve+ with the values sealed to +key+ (RollSecrets.generate_key) on the server and opened
    # here: they are never in the clear between the two.
    def resolve_sealed(env, consumer, key, path: "")
      r = call("POST", env_path(env) + "/resolve", { "consumer" => consumer, "path" => or_root(path), "seal_to" => RollSecrets.public_key(key) })
      opened = JSON.parse(RollSecrets.open_sealed(r["sealed"], key, r["context"]))
      Values.of(r, opened)
    end

    # Moves whenever +env+'s values under +path+ would; reads no value.
    def digest(env, path: "")
      call("GET", env_path(env) + "/digest" + query(path))["digest"]
    end

    # The project's encryption key +name+ (docs/secrets.md, "Encryption keys"): data is encrypted,
    # signed and HMACed by roll, and the key never leaves it.
    def transit(name)
      Transit.new(self, name)
    end

    # Pushes a dynamic lease's expiry out by +ttl_minutes+ (its link's TTL by default).
    def renew(lease, ttl_minutes: nil)
      body = ttl_minutes ? { "ttl_minutes" => ttl_minutes } : {}
      call("POST", "/v1/secrets/projects/#{esc(project_id)}/leases/#{esc(lease)}/renew", body)
    end

    # Yields the values now, then again whenever the digest moves, renewing the latest values'
    # dynamic leases meanwhile (as +roll run --watch+ does), until +stop+ (a RollSecrets::Stop) is
    # set. Values that don't resolve never reach the block: what's running keeps the previous ones.
    # Errors go to +on_error+ and the watch goes on.
    def watch(env, path: "", consumer: nil, every: 5.0, renew_every: 600.0, on_error: nil, stop: nil, &on_change)
      raise ArgumentError, "watch takes a block" unless on_change
      stop ||= Stop.new
      fail_with = on_error || ->(_e) {}
      last = nil
      leases = []
      renewed = 0.0
      waited = 0.0
      # The digest moved, so a kept answer is the old one: only what changed since the values this
      # watch holds (+…/resolve+ has no delta route, so a consumer resolves the folder).
      answer = Held.empty
      until stop.set?
        begin
          now = digest(env, path: path)
          if now != last
            v = consumer ? resolve(env, consumer, path: path) : delta_values(env, path, answer)
            before = last
            last = now
            unless v.problems.empty?
              last = nil if before.nil? # nothing running yet: try again next time
              p = v.problems.first
              raise SecretsError.new("#{env} doesn't resolve: #{p.key}: #{p.problem}", 409, "unresolved")
            end
            leases = v.leases
            on_change.call(v)
          end
        rescue StandardError => e # the watch goes on, as documented
          fail_with.call(e)
        end
        break if stop.wait(every)
        waited += every
        next unless waited - renewed >= renew_every
        renewed = waited
        leases.each do |lease|
          renew(lease)
        rescue StandardError => e
          fail_with.call(e)
        end
      end
    end

    # One path segment, percent-encoded. :nodoc:
    def self.escape(s)
      URI.encode_www_form_component(s.to_s).gsub("+", "%20")
    end

    # A request with the client's token, its answer parsed. :nodoc:
    def call(method, path, body = nil)
      authed { |token| send_request(token, method, path, body) }
    end

    # One request with +token+: the parsed answer, or a SecretsError. :nodoc:
    def send_request(token, method, path, body = nil)
      data = body.nil? ? nil : JSON.generate(body)
      headers = {}
      headers["Authorization"] = "Bearer #{token}" unless token.to_s.empty?
      headers["Content-Type"] = "application/json" if data
      status, text, _etag, reason = request(method, @api + path, headers, data)
      raise error(status, reason, text) unless status.between?(200, 299)
      text.empty? ? nil : JSON.parse(text)
    end

    private

    # +yield+s the client's token. A logged-in client's is renewed first when it's ending, and when
    # the server refuses it (revoked, ended early): one new login, then once more.
    def authed
      return yield(@token) if @relogin.nil?
      token = @relogin.token(self)
      begin
        yield(token)
      rescue SecretsError => e
        raise unless e.status == 401
        yield(@relogin.token(self, stale: token))
      end
    end

    # Kept for +cache_ttl+, then asks for what changed since the kept answer (+…/values/delta+; the
    # full set when nothing is kept). Where that fails, a conditional read: a 304 keeps the values.
    # +e+ is the environment's route, +q+ the query.
    def cached(e, q)
      url = e + "/values" + q
      hit = @cache_lock.synchronize { @cache[url] }
      return hit[0].values if hit && @clock.call - hit[1] < @cache_ttl
      off = hit ? hit[0].off : false
      if !off && !@no_delta
        h = hit ? hit[0].dup : Held.empty
        v = try_delta(e + "/values/delta" + q, h)
        if v
          keep(url, h)
          return v
        end
        off = h.off
      end
      authed { |token| revalidate(url, token, hit, off) }
    end

    # One conditional read of +url+ with +token+: a 304 keeps +hit+'s values. +off+: the folder's
    # claims are past what rolld takes, kept with the answer.
    def revalidate(url, token, hit, off)
      headers = {}
      headers["Authorization"] = "Bearer #{token}" unless token.to_s.empty?
      headers["If-None-Match"] = "\"#{hit[0].digest}\"" if hit && !hit[0].digest.empty?
      status, text, etag, reason = request("GET", @api + url + (url.include?("?") ? "&" : "?") + "digest=1", headers, nil)
      if status == 304 && hit
        k = hit[0].dup
        k.off = off
        keep(url, k)
        return k.values
      end
      raise error(status, reason, text) unless status.between?(200, 299)
      v = Values.of(JSON.parse(text))
      etag = etag.to_s.delete_prefix("W/").delete('"')
      v = v.with(v.values, etag) if v.digest.empty?
      keep(url, Held.new(v, etag, "", {}, off))
      v
    end

    # Keeps an answer for +cache_ttl+, unless it holds dynamic credentials (they end with their
    # lease) or a key that didn't resolve (the digest may not move when the link recovers).
    def keep(url, held)
      @cache_lock.synchronize do
        if !held.values.dynamic.empty? || !held.values.problems.empty?
          @cache.delete(url)
        else
          @cache[url] = [held, @clock.call]
        end
      end
    end

    # A fresh read of the values through the delta route, keeping the answer in +held+. Any failure
    # of the delta reads in full (a rolld without the route: from then on).
    def delta_values(env, path, held)
      if !held.off && !@no_delta
        e = env_path(env)
        v = try_delta(e + "/values/delta" + query(path), held)
        if v
          # The newest answer: a kept one for the folder goes.
          @cache_lock.synchronize { @cache.delete(e + "/values" + query(path)) }
          return Marshal.load(Marshal.dump(v))
        end
      end
      values(env, path: path, fresh: true)
    end

    # +read_delta+, or nil where it fails and the caller reads in full instead: a rolld without the
    # route (404, 405) from then on, a claim past what it takes (413) for that folder, anything else
    # this once.
    def try_delta(url, held)
      read_delta(url, held)
    rescue SecretsError => e
      if [404, 405].include?(e.status)
        @no_delta = true
      elsif e.status == 413
        held.off = true
      end
      nil
    rescue StandardError
      nil
    end

    # Asks +url+ (+…/values/delta?…+) for what changed since the answer +held+ holds and merges it
    # in, leaving the new answer in +held+: the full set when it holds nothing it may claim (nothing
    # yet, problems, no basis) or when rolld answers it. On any failure +held+ holds nothing, so the
    # next claim asks for the full set.
    def read_delta(url, held)
      claim = {}
      if held.values && !held.basis.empty? && held.values.problems.empty?
        claim = { "digest" => held.digest, "basis" => held.basis, "stamps" => held.stamps }
      end
      begin
        a = call("POST", url, claim) || {}
      rescue StandardError
        held.clear!
        raise
      end
      v = Values.of(a)
      stamps = (a["stamps"] || {}).to_h
      values = v.values
      if a["delta"] == true && held.values
        values = held.values.values.dup
        kept = held.stamps.dup
        (a["removed"] || []).each do |k|
          values.delete(k)
          kept.delete(k)
        end
        values.merge!(v.values)
        stamps = kept.merge!(stamps)
      end
      v = v.with(values, a["digest"].to_s)
      held.values = v
      held.digest = v.digest
      held.basis = a["basis"].to_s
      held.stamps = stamps
      v
    end

    # [status, body, ETag, reason]: a success's body read up to ANSWER_LIMIT (more is an error), an
    # error's up to ERROR_LIMIT (the rest isn't read).
    def request(method, url, headers, data)
      uri = URI(url)
      req = Net::HTTPGenericRequest.new(method, !data.nil?, true, uri.request_uri, headers)
      req.body = data if data
      Net::HTTP.start(uri.host, uri.port, use_ssl: uri.scheme == "https", open_timeout: @timeout, read_timeout: @timeout, write_timeout: @timeout) do |http|
        http.request(req) do |res|
          status = res.code.to_i
          limit = status.between?(200, 299) ? ANSWER_LIMIT : ERROR_LIMIT
          text = String.new
          catch(:full) do
            res.read_body do |chunk|
              room = limit - text.bytesize
              if chunk.bytesize > room
                raise SecretsError.new("the answer is larger than #{ANSWER_LIMIT >> 20} MiB", 502) if limit == ANSWER_LIMIT
                text << chunk.byteslice(0, room)
                throw :full
              end
              text << chunk
            end
          end
          return [status, text.force_encoding(Encoding::UTF_8), res["ETag"], res.message]
        end
      end
    end

    # The API's error and code, or the status when the body isn't one.
    def error(status, reason, text)
      err = begin
        JSON.parse(text.empty? ? "{}" : text)
      rescue JSON::ParserError
        {}
      end
      err = {} unless err.is_a?(Hash)
      message = err["error"].to_s.empty? ? "#{status} #{reason}".strip : err["error"]
      SecretsError.new(message, status, err["code"])
    end

    def env_path(env)
      "/v1/secrets/projects/#{esc(project_id)}/environments/#{esc(env)}"
    end

    def query(path)
      path.to_s.empty? ? "" : "?path=#{esc(path)}"
    end

    def or_root(path)
      path.to_s.empty? ? "/" : path
    end

    def esc(s)
      Client.escape(s)
    end
  end
end
