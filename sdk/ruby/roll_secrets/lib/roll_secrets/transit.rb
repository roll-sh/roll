# frozen_string_literal: true

module RollSecrets
  # One encryption key, as @roll/secrets' transit(name): bytes or text in, base64 on the wire, bytes
  # (a binary String) back from +decrypt+.
  class Transit
    attr_reader :name

    def initialize(client, name)
      @client = client
      @name = name
    end

    def inspect
      "#<#{self.class.name} name=#{@name.inspect}>"
    end
    alias to_s inspect

    # The ciphertext (+roll:v<n>:…+); +context+, when given, must be given to decrypt it.
    def encrypt(data, context = nil)
      post("encrypt", { "plaintext" => b64(data), "context" => context })["ciphertext"]
    end

    # The plaintext (audited, as a reveal).
    def decrypt(ciphertext, context = nil)
      post("decrypt", { "ciphertext" => ciphertext, "context" => context })["plaintext"].unpack1("m")
    end

    # An Ed25519 signature over the bytes as given: anyone with +public_keys+ checks it.
    def sign(data)
      post("sign", { "input" => b64(data) })["signature"]
    end

    def verify(data, signature)
      post("verify", { "input" => b64(data), "signature" => signature })["valid"] == true
    end

    # HMAC-SHA256, the same for the same input: a lookup column over encrypted fields.
    def hmac(data)
      post("hmac", { "input" => b64(data) })["hmac"]
    end

    # An Ed25519 key's public halves, every version (+public_key+, +pem+, +jwk+).
    def public_keys
      @client.call("GET", at("public"))
    end

    private

    def at(op)
      "/v1/secrets/projects/#{Client.escape(@client.project_id)}/transit/#{Client.escape(@name)}/#{op}"
    end

    def post(op, body)
      @client.call("POST", at(op), body)
    end

    # Text as its UTF-8 bytes, a binary String as it is.
    def b64(data)
      [data.to_s.b].pack("m0")
    end
  end
end
