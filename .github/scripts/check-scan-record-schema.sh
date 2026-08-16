#!/bin/sh
# The scan-record schema lives in two places — the README's "Scan record
# template" (canonical) and SKILL.md's "Committed scan record" (what a closing
# run has in front of it) — and it is documentation, so nothing else in this
# repo executes it. That is how it shipped with no field naming the tree its
# after-campaign counts hold at: rain.sol.codegen committed "testsAfter": 102, a
# count occurring at no commit in the range its record covered, and no reader or
# tool had anything to check it against.
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

# The template exemplifies the field; only the prose states the RULES it obeys,
# and a record is falsifiable because of the rules, not because of the field. A
# check that accepted the name alone would pass a README that had kept
# `testsAfterCommit` in one sentence and dropped every rule attached to it, so
# each rule is pinned by the shortest phrase that carries it. Reword freely — the
# failure names which rule went missing, and re-pinning it is a one-line edit.
readme_prose=$(awk '
	/^## Scan record template$/ { in_section = 1; next }
	in_section && /^## / { exit }
	in_section && /^```/ { fenced = !fenced; next }
	in_section && !fenced { print }
' "$readme")

[ -n "$readme_prose" ] || fail "README.md has no prose under '## Scan record template' — only the template"

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

echo "scan record schema OK"
