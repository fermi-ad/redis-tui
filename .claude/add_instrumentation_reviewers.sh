#!/usr/bin/env bash
# Automatically adds the fermi-ad instrumentation team as reviewers after gh pr create.
# Team membership is fetched live from GitHub each time - no names are hardcoded.
INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // ""')

if echo "$COMMAND" | grep -q 'gh pr create'; then
  PR_URL=$(echo "$INPUT" | jq -r '(.tool_response.stdout // .tool_response // "") | if type == "string" then . else "" end' \
    | grep -oE 'https://github\.com/[^/]+/[^/]+/pull/[0-9]+' | head -1)
  if [ -n "$PR_URL" ]; then
    PR_NUM=$(echo "$PR_URL" | grep -oE '[0-9]+$')
    CURRENT_USER=$(gh api user --jq '.login' 2>/dev/null)
    REVIEWERS=$(gh api orgs/fermi-ad/teams/instrumentation/members --jq '.[].login' 2>/dev/null \
      | grep -v "$CURRENT_USER" | tr '\n' ',' | sed 's/,$//')
    if [ -n "$REVIEWERS" ]; then
      REPO=$(gh repo view --json nameWithOwner --jq '.nameWithOwner' 2>/dev/null)
    gh pr edit "$PR_NUM" --repo "$REPO" --add-reviewer "$REVIEWERS" 2>/dev/null || true
    fi
  fi
fi
