# frozen_string_literal: true

module RollSecrets
  # An answer that wasn't a success: its status, roll's code and message. Never a value.
  class SecretsError < StandardError
    attr_reader :status, :code

    def initialize(message, status, code = nil)
      super(message)
      @status = status
      @code = code
    end
  end

  Problem = Struct.new(:key, :problem)

  # A key left out because it links a resource the environment's preview branched: roll previews
  # sets +name+ from its branch's +field+.
  Followed = Struct.new(:name, :kind, :parent, :field)

  # An environment's values. +dynamic+ keys hold a credential that ends with its lease (+leases+;
  # +renew+ keeps it); +linked+ keys come from a link to a resource; +followed+ keys link a resource
  # a preview branched, and aren't in +values+; +withheld+ are sensitive keys left out (only roll's
  # runtimes and syncs read them). +digest+ is the folder's digest the values were read against,
  # when the read asked for it (a cached or delta read does).
  #
  # +inspect+ and +to_s+ name the keys, never the values, so a log line or an error report doesn't
  # carry one.
  class Values
    attr_reader :values, :problems, :leases, :dynamic, :linked, :followed, :withheld, :digest

    def initialize(values:, problems: [], leases: [], dynamic: [], linked: [], followed: [], withheld: [], digest: "")
      @values = values
      @problems = problems
      @leases = leases
      @dynamic = dynamic
      @linked = linked
      @followed = followed
      @withheld = withheld
      @digest = digest
    end

    # These values with +values+ in place of their own, read against +digest+. :nodoc:
    def with(values, digest)
      Values.new(values: values, problems: @problems, leases: @leases, dynamic: @dynamic, linked: @linked,
                 followed: @followed, withheld: @withheld, digest: digest)
    end

    def self.of(d, values = nil)
      d ||= {}
      new(
        values: values || (d["values"] || {}).to_h,
        problems: (d["problems"] || []).map { |p| Problem.new(p["key"], p["problem"]) },
        leases: (d["leases"] || []).to_a,
        dynamic: (d["dynamic"] || []).to_a,
        linked: (d["linked"] || []).to_a,
        followed: (d["followed"] || []).map { |f| Followed.new(f["name"], f["kind"], f["parent"], f["field"]) },
        withheld: (d["withheld"] || []).to_a,
        digest: d["digest"].to_s
      )
    end

    def inspect
      "#<#{self.class.name} keys=#{@values.keys.inspect} problems=#{@problems.map(&:key).inspect} " \
        "leases=#{@leases.inspect} dynamic=#{@dynamic.inspect} linked=#{@linked.inspect}>"
    end
    alias to_s inspect
  end

  # A folder's last answer with every key's stamp, so a read after a change asks only for what
  # changed (+…/values/delta+, docs/secrets.md "Delta reads"): what a watch keeps, and what
  # +cache_ttl+ keeps. +off+: the folder's claim is past what rolld takes (a 413); its reads are
  # full. :nodoc:
  Held = Struct.new(:values, :digest, :basis, :stamps, :off) do
    def self.empty
      new(nil, "", "", {}, false)
    end

    def clear!
      self.values = nil
      self.digest = ""
      self.basis = ""
      self.stamps = {}
    end
  end
end
