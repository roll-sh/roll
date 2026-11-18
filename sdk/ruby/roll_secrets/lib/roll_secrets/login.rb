# frozen_string_literal: true

require "json"
require "net/http"
require "uri"

module RollSecrets
  # Keeps a logged-in client's token fresh: a new login once less than a fifth of its life (at least
  # a minute) is left, or when the server refuses it. :nodoc:
  class Relogin
    def initialize(identity, source)
      @identity = identity
      @source = source
      @lock = Mutex.new
      @issued = 0.0
      @expires = 0.0
    end

    def inspect
      "#<#{self.class.name} identity=#{@identity.inspect}>"
    end

    def token(c, stale: nil)
      @lock.synchronize do
        margin = [60.0, (@expires - @issued) / 5].max
        # An answer without an expiry is renewed only when the server refuses it.
        ending = @expires.positive? && Time.now.to_f > @expires - margin
        refused = stale == c.token
        if refused || ending
          begin
            r = Client.new(c.api, "", timeout: c.timeout).send_request("", "POST", "/v1/secrets/login", { "identity" => @identity, "token" => @source.call.to_s.strip })
          rescue StandardError
            # Renewing early that fails keeps the token it has until it really ends.
            raise if refused || Time.now.to_f >= @expires
            return c.token
          end
          raise SecretsError.new("the login answered no token", 502) unless r.is_a?(Hash) && !r["token"].to_s.empty?
          c.token = r["token"]
          @issued = Time.now.to_f
          @expires = (r["expires_at"] || 0) / 1000.0
          c.project = r["project"] if c.project.to_s.empty?
        end
        c.token
      end
    end
  end

  module_function

  # Exchanges a workload's own OIDC token for a short-lived roll token through workload identity
  # (docs/secrets.md, "Workload identities"): nothing long-lived in the workload's settings. The
  # client logs in again with the same OIDC token before its roll token ends (+from_env+ reads a
  # fresh OIDC token each time).
  def login(api, identity, oidc_token)
    logged_in(api, identity, -> { oidc_token })
  end

  # A client as +roll run+ makes one: ROLL_API_URL, then ROLL_TOKEN, or ROLL_IDENTITY with the
  # workload's OIDC token from ROLL_OIDC_TOKEN, ROLL_OIDC_TOKEN_FILE (a Kubernetes projected token)
  # or GitHub Actions (+permissions: id-token: write+; audience ROLL_OIDC_AUDIENCE, default the API's
  # URL). ROLL_PROJECT names the project when the token doesn't.
  def from_env
    env = ->(k) { ENV.fetch(k, "").strip }
    api = env.call("ROLL_API_URL")
    raise SecretsError.new("ROLL_API_URL isn't set", 400, "config") if api.empty?
    c = if !env.call("ROLL_TOKEN").empty?
      Client.new(api, env.call("ROLL_TOKEN"))
    elsif !env.call("ROLL_IDENTITY").empty?
      # The OIDC token is read again for each login (a projected token is replaced on disk).
      logged_in(api, env.call("ROLL_IDENTITY"), -> { oidc_token(api, env) })
    else
      raise SecretsError.new("set ROLL_TOKEN, or ROLL_IDENTITY for a workload identity", 400, "config")
    end
    c.project = env.call("ROLL_PROJECT") unless env.call("ROLL_PROJECT").empty?
    c
  end

  def logged_in(api, identity, source)
    c = Client.new(api, "")
    c.relogin = Relogin.new(identity, source)
    c.relogin.token(c, stale: "")
    c
  end
  private_class_method :logged_in

  def oidc_token(api, env)
    return env.call("ROLL_OIDC_TOKEN") unless env.call("ROLL_OIDC_TOKEN").empty?
    return File.read(env.call("ROLL_OIDC_TOKEN_FILE"), encoding: "UTF-8") unless env.call("ROLL_OIDC_TOKEN_FILE").empty?
    url = env.call("ACTIONS_ID_TOKEN_REQUEST_URL")
    bearer = env.call("ACTIONS_ID_TOKEN_REQUEST_TOKEN")
    if url.empty? || bearer.empty?
      raise SecretsError.new("ROLL_IDENTITY is set but there's no OIDC token: set ROLL_OIDC_TOKEN or ROLL_OIDC_TOKEN_FILE, or run in GitHub Actions with `permissions: id-token: write`", 400, "config")
    end
    # The server's own URL by default: a token minted for one roll server isn't good at another.
    audience = env.call("ROLL_OIDC_AUDIENCE")
    audience = api.sub(%r{/+\z}, "") if audience.empty?
    value = begin
      uri = URI(url + (url.include?("?") ? "&" : "?") + "audience=" + URI.encode_www_form_component(audience))
      req = Net::HTTP::Get.new(uri.request_uri, "Authorization" => "Bearer #{bearer}")
      body = Net::HTTP.start(uri.host, uri.port, use_ssl: uri.scheme == "https", open_timeout: 30, read_timeout: 30) do |http|
        http.request(req) do |res|
          text = String.new
          catch(:full) do
            res.read_body do |chunk|
              text << chunk
              throw :full if text.bytesize >= 64 << 10
            end
          end
          break res.is_a?(Net::HTTPSuccess) ? text.byteslice(0, 64 << 10) : nil
        end
      end
      body && JSON.parse(body)["value"]
    rescue StandardError
      nil
    end
    raise SecretsError.new("GitHub Actions didn't give its OIDC token (does the job have `permissions: id-token: write`?)", 502, "oidc") if value.to_s.empty?
    value
  end
  private_class_method :oidc_token
end
