#!/usr/bin/env bash
# Checks that a collaborator's pull request comes from a branch made from an
# issue they opened themselves.
#
# The "new branches" ruleset already refuses, at push time, any collaborator
# branch whose name does not start with "<number>-" (the name GitHub's
# "Create a branch" button on an issue gives). A ruleset only sees the name,
# though: it cannot tell whether issue <number> exists, is still open, or was
# opened by someone else. This script checks those three things.
#
# Pull requests from the repository owner and from Dependabot are not
# checked: the owner bypasses the rulesets on purpose, and Dependabot names
# its own branches.
#
# Inputs (environment): GH_TOKEN, REPO (owner/name), OWNER, AUTHOR (the pull
# request's author), BRANCH (its head branch). BRANCH comes from whoever
# opened the pull request, so it is only ever read from the environment and
# matched against a regex, never pasted into a command.
set -euo pipefail

if [[ "$AUTHOR" == "$OWNER" || "$AUTHOR" == "dependabot[bot]" ]]; then
  echo "Not checked: opened by $AUTHOR."
  exit 0
fi

fail() {
  echo "::error::$1"
  exit 1
}

[[ "$BRANCH" =~ ^([0-9]+)- ]] ||
  fail "Branch '$BRANCH' does not start with an issue number. Open an issue, then use its 'Create a branch' button."
number=${BASH_REMATCH[1]}

# The issues endpoint also answers for pull request numbers; those carry a
# "pull_request" key and are refused below.
issue=$(gh api "repos/$REPO/issues/$number" 2>/dev/null) ||
  fail "Issue #$number (from branch '$BRANCH') does not exist."

[[ $(jq -r '.pull_request != null' <<<"$issue") == false ]] ||
  fail "#$number is a pull request, not an issue."
[[ $(jq -r '.state' <<<"$issue") == open ]] ||
  fail "Issue #$number is closed."
opener=$(jq -r '.user.login' <<<"$issue")
[[ "$opener" == "$AUTHOR" ]] ||
  fail "Issue #$number was opened by $opener, not by $AUTHOR. Work on an issue you opened."

echo "OK: branch '$BRANCH' comes from issue #$number, opened by $AUTHOR."
