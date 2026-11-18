# frozen_string_literal: true

require "json"
require "socket"

# A fake rolld on a free local port: one request per connection, answered by the block
# (method, path, headers, body) -> [status, headers, body]. WEBrick isn't in the standard library
# since Ruby 3.0, and HTTP/1.0-style answers are all the client needs.
class FakeServer
  REASONS = { 200 => "OK", 304 => "Not Modified", 400 => "Bad Request", 401 => "Unauthorized", 404 => "Not Found" }.freeze

  attr_reader :url

  def initialize(&handler)
    @handler = handler
    @server = TCPServer.new("127.0.0.1", 0)
    @url = "http://127.0.0.1:#{@server.addr[1]}"
    @thread = Thread.new { loop { serve(@server.accept) } rescue IOError }
  end

  def close
    @server.close
    @thread.join
  end

  def self.json(status, body = nil, headers = {})
    [status, { "Content-Type" => "application/json" }.merge(headers), body.nil? ? "" : JSON.generate(body)]
  end

  private

  def serve(sock)
    method, target = sock.gets.to_s.split(" ")
    headers = {}
    while (line = sock.gets) && line != "\r\n"
      k, v = line.split(":", 2)
      headers[k.strip.downcase] = v.strip
    end
    body = sock.read(headers["content-length"].to_i)
    status, out_headers, out = @handler.call(method, target, headers, body.to_s.empty? ? {} : JSON.parse(body))
    sock.write("HTTP/1.1 #{status} #{REASONS.fetch(status, "Status")}\r\n")
    out_headers.merge("Content-Length" => out.bytesize.to_s, "Connection" => "close").each { |k, v| sock.write("#{k}: #{v}\r\n") }
    sock.write("\r\n#{out}")
  rescue StandardError => e
    warn "fake server: #{e.class}: #{e.message}"
  ensure
    sock.close
  end
end
