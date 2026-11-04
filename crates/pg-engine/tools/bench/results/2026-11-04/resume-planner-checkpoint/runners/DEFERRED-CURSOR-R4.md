# Deferred cursor work; not part of independent165

Original work-a165 remains7ffaee17178ae4dfe4819106de2a9e3574cf7a15 and remote-backed.
Parent R4 terminal review:both R3 medium bugs fixed,no newHIGH/MEDIUM correctness bug;
selective default scroll O(scanned) remainsMEDIUM/unaccepted. Parent chose independent
lock/SearchPath/session package instead. No deferred cursor benchmark/source acceptance.

Future smaller option than broadspill:qual-free replayable input scan may discard forward
raw rows and restart prefix without evaluating qual;qual_move must use base offsets,then
retain afterfirstbackward. Needs separate scoped design/runtime red proofs,not thisbatch.

Preserve LOW from_call observable WHERE backward recheck gap, plus PL constant-refcursor/
FDW error-order hypothesis for later171/172. No expansion of independent165scope.
Reference parent cache/oltp-source-review-r4-result.json; all local experiments and focused
red/green,actualPGreference/source identities remain in exported evidence branch.
7ffa fulldebug stage explicitlySUPERSEDED,incomplete;laterstagesNOTRUN;exact ownedPID
cleanup receipt check165r3/SUPERSEDED.json. Focus14cursorfiles×3 +wrapper selftestpassed.
