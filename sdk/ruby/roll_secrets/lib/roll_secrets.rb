# frozen_string_literal: true

# A client for roll secrets (docs/secrets.md, "Consumption"), with the same surface as
# @roll/secrets, sdk/go/secrets and sdk/python/roll_secrets: an environment's values, the runtime
# resolve (sealed to a key only this process holds, if asked), the digest that moves when the values
# would, a watch that follows it and renews dynamic leases, and transit encryption keys.
#
# The standard library only (net/http, json, openssl).
#
# A client holds a roll token (+rlt_…+, roll-access's) with a rule on one project, the operator
# token, or the short-lived token a workload identity's login gives (+login+, +from_env+).
module RollSecrets
end

require_relative "roll_secrets/version"
require_relative "roll_secrets/values"
require_relative "roll_secrets/seal"
require_relative "roll_secrets/stop"
require_relative "roll_secrets/client"
require_relative "roll_secrets/transit"
require_relative "roll_secrets/login"
