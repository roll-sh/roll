# frozen_string_literal: true

require_relative "lib/roll_secrets/version"

Gem::Specification.new do |s|
  s.name = "roll_secrets"
  s.version = RollSecrets::VERSION
  s.summary = "A client for roll secrets: values, sealed resolves, digests, watching for changes and transit keys"
  s.description = "The same surface as @roll/secrets and roll's Go and Python clients, on Ruby's standard library only (net/http, json, openssl)."
  s.authors = ["roll"]
  s.files = Dir["lib/**/*.rb"] + ["README.md"]
  s.require_paths = ["lib"]
  # X25519 through OpenSSL::PKey and OpenSSL::KDF.hkdf: the openssl gem Ruby 3.0 ships.
  s.required_ruby_version = ">= 3.0"
end
