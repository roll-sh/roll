# frozen_string_literal: true

require "openssl"

module RollSecrets
  # roll-seal-v1 boxes, as crates/roll-secrets/src/crypto.rs makes them: X25519 between a fresh key
  # pair and the recipient's key, HKDF-SHA256 with salt epk ‖ recipient and info "roll-seal-v1" into
  # an AES-256-GCM key, a random 12-byte nonce, and the context as the associated data.
  #
  # Ruby's own openssl does all of it. Keys go in and out through DER rather than the raw-key
  # methods, which only openssl gem 3.1 and later have.
  module Seal
    INFO = "roll-seal-v1"
    PRIVATE_DER = ["302e020100300506032b656e04220420"].pack("H*").freeze
    PUBLIC_DER = ["302a300506032b656e032100"].pack("H*").freeze

    module_function

    def b64(bytes)
      [bytes].pack("m0").tr("+/", "-_").delete("=")
    end

    def unb64(s)
      s = s.to_s.tr("-_", "+/")
      (s + "=" * (-s.length % 4)).unpack1("m0")
    end

    # An X25519 key from its 32 raw secret bytes.
    def private_key(raw)
      raise ArgumentError, "an X25519 secret key is 32 bytes" unless raw.bytesize == 32
      OpenSSL::PKey.read(PRIVATE_DER + raw.b)
    end

    def public_bytes(key)
      key.public_to_der.byteslice(PUBLIC_DER.bytesize, 32)
    end

    def public_from(raw)
      OpenSSL::PKey.read(PUBLIC_DER + raw.b)
    end
  end

  module_function

  # An X25519 key a sealed resolve is sealed to; keep it in the process.
  def generate_key
    OpenSSL::PKey.generate_key("X25519")
  end

  # The base64url public key of +key+: what +resolve_sealed+ sends.
  def public_key(key)
    Seal.b64(Seal.public_bytes(key))
  end

  # Opens a box (+v+, +epk+, +nonce+, +ct+) sealed to +key+ for +context+; raises ArgumentError when
  # it doesn't open.
  def open_sealed(box, key, context)
    box = box.transform_keys(&:to_s)
    epk = Seal.unb64(box.fetch("epk"))
    nonce = Seal.unb64(box.fetch("nonce"))
    ct = Seal.unb64(box.fetch("ct"))
    raise ArgumentError unless box["v"] == 1 && epk.bytesize == 32 && nonce.bytesize == 12 && ct.bytesize >= 16
    shared = key.derive(Seal.public_from(epk))
    raise ArgumentError if shared.bytes.all?(&:zero?)
    k = OpenSSL::KDF.hkdf(shared, salt: epk + Seal.public_bytes(key), info: Seal::INFO, length: 32, hash: "SHA256")
    c = OpenSSL::Cipher.new("aes-256-gcm").decrypt
    c.key = k
    c.iv = nonce
    c.auth_tag = ct.byteslice(-16, 16)
    c.auth_data = context.to_s.b
    c.update(ct.byteslice(0, ct.bytesize - 16)) + c.final
  rescue StandardError
    # Any failure is the same answer.
    raise ArgumentError, "the box doesn't open"
  end
end
