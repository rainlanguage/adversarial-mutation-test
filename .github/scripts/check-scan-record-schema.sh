#!/bin/sh
# The scan-ledger schema lives in two places — the README's "Scan ledger"
# (canonical) and SKILL.md's "Committed scan record" (what a closing run has in
# front of it) — and it is documentation, so nothing else in this repo executes
# it. That is how it shipped with no field naming the tree its after-campaign
# counts hold at: rain.sol.codegen committed "testsAfter": 102, a count
# occurring at no commit in the range its record covered, and no reader or tool
# had anything to check it against. It is also how the ledger shipped as a bare
# array with no stated ordering rule, so that same repo's record still reads
# "commitsAheadOfTag": 0 from 35 commits and 5 tags behind its default branch.
#
# This pins the parts of the schema a reader needs in order to falsify a record:
# the ledger is a wrapper object that can carry its own read rule, the template
# is real JSON, both trees are named, both are full SHAs, and both documents
# state it. Run from anywhere: sh .github/scripts/check-scan-record-schema.sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
readme="$root/README.md"
skill="$root/skills/adversarial-mutation-test/SKILL.md"

fail() {
	echo "FAIL $*" >&2
	exit 1
}

# The fenced JSON block under the README's "## Scan ledger" heading.
ledger=$(awk '
	/^## Scan ledger$/ { in_section = 1; next }
	in_section && /^## / { exit }
	in_section && /^```json$/ { in_block = 1; next }
	in_block && /^```$/ { exit }
	in_block { print }
' "$readme")

[ -n "$ledger" ] || fail "README.md has no fenced json block under '## Scan ledger'"

echo "$ledger" | jq -e . >/dev/null 2>&1 ||
	fail "the README scan ledger template is not valid JSON — it is the schema consumers copy"

# The envelope. A bare array structurally cannot carry a schema version or an
# ordering invariant, so the rule for reading it could only live in prose that
# does not ship with the file — which is exactly how a record 35 commits stale
# stayed indistinguishable from a current one.
echo "$ledger" | jq -e 'type == "object"' >/dev/null ||
	fail "the ledger template must be a wrapper object, not a bare array — a bare array cannot state its own read rule"
echo "$ledger" | jq -e '.schemaVersion | type == "number"' >/dev/null ||
	fail "the ledger template has no top-level numeric 'schemaVersion'"
echo "$ledger" | jq -e '.records | type == "array" and length > 0' >/dev/null ||
	fail "the ledger template has no non-empty 'records' array — the array a campaign appends to must be shown, not just one element of it"
envelope_keys=$(echo "$ledger" | jq -r 'keys_unsorted | join(" ")')
echo "ledger envelope keys: $envelope_keys"
echo "OK   envelope is an object carrying schemaVersion and a non-empty records array"

record=$(echo "$ledger" | jq -c '.records[0]')
keys=$(echo "$record" | jq -r 'keys_unsorted | join(" ")')
commit=$(echo "$record" | jq -r '.commit // "<absent>"')
tests_after_commit=$(echo "$record" | jq -r '.testsAfterCommit // "<absent>"')
echo "record keys: $keys"
echo "record commit=$commit testsAfterCommit=$tests_after_commit"

# The must-haves. A record missing any of these cannot be checked against the
# repo it describes. schemaVersion joins them because migration preserves older
# records untouched: its absence is what tells a reader "this one predates the
# wrapper, do not assume the current shape".
for key in schemaVersion timestamp commit testsAfterCommit publishedTag commitsAheadOfTag; do
	echo "$record" | jq -e --arg key "$key" 'has($key)' >/dev/null ||
		fail "the template record is missing must-have '$key'"
done
echo "OK   must-haves present: schemaVersion timestamp commit testsAfterCommit publishedTag commitsAheadOfTag"

# Distinct from skillVersion, which names the skill that wrote the record and
# has already failed to describe its shape.
echo "$record" | jq -e '.schemaVersion != .skillVersion' >/dev/null ||
	fail "the record's schemaVersion must be distinct from skillVersion — skillVersion is not a statement about shape"
echo "OK   record schemaVersion is distinct from skillVersion"

# Adjacency is what makes the pair readable as a pair: the before tree and the
# after tree sit together, ahead of everything measured at either.
echo "$record" | jq -e 'keys_unsorted | index("testsAfterCommit") == (index("commit") + 1)' >/dev/null ||
	fail "testsAfterCommit must come immediately after commit; key order is: $keys"
echo "OK   testsAfterCommit is immediately after commit"

# Full SHAs. A 7-char prefix is a weaker anchor than a full SHA and grows
# ambiguous as history grows — which is the same class of defect the after-tree
# field exists to close, and rain.solmem's record already carries one.
for key in commit testsAfterCommit; do
	echo "$record" | jq -e --arg key "$key" '.[$key] | type == "string" and test("^[0-9a-f]{40}$")' >/dev/null ||
		fail "template '$key' must be a full 40-character lowercase hex SHA"
done
echo "OK   commit and testsAfterCommit are full 40-character SHAs"

# The template exemplifies the field; only the prose states the RULES it obeys,
# and a record is falsifiable because of the rules, not because of the field. A
# check that accepted the name alone would pass a README that had kept
# `testsAfterCommit` in one sentence and dropped every rule attached to it, so
# each rule is pinned by the shortest phrase that carries it. Reword freely — the
# failure names which rule went missing, and re-pinning it is a one-line edit.
readme_prose=$(awk '
	/^## Scan ledger$/ { in_section = 1; next }
	in_section && /^## / { exit }
	in_section && /^```/ { fenced = !fenced; next }
	in_section && !fenced { print }
' "$readme")

[ -n "$readme_prose" ] || fail "README.md has no prose under '## Scan ledger' — only the template"

# Newlines are collapsed first: these documents are reflowed by `deno fmt`, so a
# phrase may be split across lines at any time and that is not a rule going
# missing.
states() {
	printf '%s' "$2" | tr '\n' ' ' | tr -s ' ' | grep -qF -- "$1" ||
		fail "$3 no longer states $4 (looked for '$1')"
}

readme_states() { states "$1" "$readme_prose" "the README scan record prose" "$2"; }

readme_states 'testsAfterCommit' "which field names the tree after-state counts hold at"
readme_states '_before_' "that before-numbers hold at the scanned commit"
readme_states '_after_' "that after-numbers hold at testsAfterCommit"
readme_states '40-character' "that both trees are named by full-length SHAs"
readme_states 'equal to' "that a run landing nothing sets testsAfterCommit equal to commit"
readme_states 'never null' "that testsAfterCommit is never null"
readme_states 'never omitted' "that testsAfterCommit is never omitted"
echo "OK   README prose states every testsAfterCommit rule"

# The ledger's own read rule. The file is the artifact a reader holds, so every
# rule for reading it has to be written where the file's schema is defined; a
# rule kept anywhere else is the defect this section exists to close.
readme_states 'append-only' "that records is append-only"
readme_states 'never rewritten, reordered, or removed' "that a landed record is a historical fact"
readme_states 'greatest `timestamp`' "which field is authoritative for newest"
readme_states 'not the last array element' "that append order is NOT the authority — a PR queue scrambles it"
readme_states 'later wins' "how a timestamp tie is broken"
readme_states 'Newest is not current' "that a frozen record does not decay into looking stale"
readme_states 'Neither is `skillVersion`' "that schemaVersion is distinct from skillVersion"
readme_states 'wraps it in place' "what a campaign does when it opens an existing bare array"
readme_states 'no field back-filled' "that migration preserves existing records untouched"
echo "OK   README prose states every ledger ordering, authority and migration rule"

# SKILL.md is what a run closing its record actually has in front of it. A field
# documented only in the README is a field campaigns will not write, and a rule
# only the README states is a rule the closing run does not apply.
section=$(awk '
	/^## Committed scan record$/ { in_section = 1; next }
	in_section && /^## / { exit }
	in_section { print }
' "$skill")

[ -n "$section" ] || fail "SKILL.md has no '## Committed scan record' section"

skill_states() { states "$1" "$section" "SKILL.md's '## Committed scan record'" "$2"; }

skill_states 'testsAfterCommit' "which field names the tree after-state counts hold at"
skill_states 'equal to' "what a run that landed nothing writes"
skill_states 'never null' "that testsAfterCommit is never null and never omitted"
echo "OK   SKILL.md's committed scan record states the field and its rules"

# The closing run is the only writer, so a rule the README states and SKILL.md
# does not is a rule nothing applies.
skill_states 'not a bare array' "that the ledger is a wrapper object"
skill_states 'append-only' "that records is append-only"
skill_states 'greatest `timestamp`' "which field is authoritative for newest"
skill_states 'later wins' "how a timestamp tie is broken"
skill_states 'Newest is not current' "that a frozen record does not decay into looking stale"
skill_states 'wrap it on this append' "what a run opening an existing bare array does"
echo "OK   SKILL.md's committed scan record states the ledger read and migration rules"

echo "scan ledger schema OK"
