Source checkpoint 047c4697e72ce63867257db3668a02c55c77830a

VACUUM retains stored-size fast path only for raw small tuples; fully decodes large/unknown rows and uses existing sizing::layout .0 before heap packer. Index tuple formation/parbuild unchanged. Direct immediate kept_sizes removal and forced range EXPLAIN assertions added. Focused checks pending, not ready; parent combined163 HIGH remains.
