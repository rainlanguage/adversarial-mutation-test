#!/bin/sh
# The scan-record schema lives in two places — the README's "Scan record
# template" (canonical) and SKILL.md's "Committed scan record" (what a closing
# run has in front of it) — and it is documentation, so nothing else in this
# repo executes it. That is how the schema shipped for 33 versions with no field
# naming the tree its after-campaign counts hold at: rain.sol.codegen committed
# "testsAfter": 102, a count occurring at no commit in the range its record
# covered, and no reader or tool had anything to check it against.
#
# This pins the parts of the schema a reader needs in order to falsify a record:
# the template is real JSON, both trees are named, both are full SHAs, and both
# documents state it. Run it from anywhere: sh .github/scripts/check-scan-record-schema.sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
readme="$root/README.md"
skill="$root/skills/adversarial-mutation-test/SKILL.md"

fail() {
	echo "FAIL $*" >&2
	exit 1
}

# The fenced JSON block under the README's "## Scan record template" heading.
template=$(awk '
	/^## Scan record template$/ { in_section = 1; next }
	in_section && /^## / { exit }
	in_section && /^```json$/ { in_block = 1; next }
	in_block && /^```$/ { exit }
	in_block { print }
' "$readme")

[ -n "$template" ] || fail "README.md has no fenced json block under '## Scan record template'"

echo "$template" | jq -e . >/dev/null 2>&1 ||
	fail "the README scan record template is not valid JSON — it is the schema consumers copy"

keys=$(echo "$template" | jq -r 'keys_unsorted | join(" ")')
commit=$(echo "$template" | jq -r '.commit // "<absent>"')
tests_after_commit=$(echo "$template" | jq -r '.testsAfterCommit // "<absent>"')
echo "template keys: $keys"
echo "template commit=$commit testsAfterCommit=$tests_after_commit"

# The must-haves. A record missing any of these cannot be checked against the
# repo it describes.
for key in timestamp commit testsAfterCommit publishedTag commitsAheadOfTag; do
	echo "$template" | jq -e --arg key "$key" 'has($key)' >/dev/null ||
		fail "the template is missing must-have '$key'"
done
echo "OK   must-haves present: timestamp commit testsAfterCommit publishedTag commitsAheadOfTag"

# Adjacency is what makes the pair readable as a pair: the before tree and the
# after tree sit together, ahead of everything measured at either.
echo "$template" | jq -e 'keys_unsorted | index("testsAfterCommit") == (index("commit") + 1)' >/dev/null ||
	fail "testsAfterCommit must come immediately after commit; key order is: $keys"
echo "OK   testsAfterCommit is immediately after commit"

# Full SHAs. A 7-char prefix is a weaker anchor than a full SHA and grows
# ambiguous as history grows — which is the same class of defect the after-tree
# field exists to close, and rain.solmem's record already carries one.
for key in commit testsAfterCommit; do
	echo "$template" | jq -e --arg key "$key" '.[$key] | type == "string" and test("^[0-9a-f]{40}$")' >/dev/null ||
		fail "template '$key' must be a full 40-character lowercase hex SHA"
done
echo "OK   commit and testsAfterCommit are full 40-character SHAs"

# The template alone only exemplifies the field; the prose around it is what
# states the rule (which numbers hold at which tree, and that a run landing
# nothing sets them equal), so the field has to appear outside the fences too.
prose=$(awk '/^```/ { fenced = !fenced; next } !fenced' "$readme")
echo "$prose" | grep -q 'testsAfterCommit' ||
	fail "README.md names testsAfterCommit only inside a code fence — the rule it obeys is unstated"
echo "OK   README prose states the testsAfterCommit rule"

# SKILL.md is what a run closing its record actually has in front of it. A field
# documented only in the README is a field campaigns will not write.
section=$(awk '
	/^## Committed scan record$/ { in_section = 1; next }
	in_section && /^## / { exit }
	in_section { print }
' "$skill")

[ -n "$section" ] || fail "SKILL.md has no '## Committed scan record' section"

echo "$section" | grep -q 'testsAfterCommit' ||
	fail "SKILL.md's '## Committed scan record' does not name testsAfterCommit"
echo "OK   SKILL.md's committed scan record names testsAfterCommit"

echo "scan record schema OK"
