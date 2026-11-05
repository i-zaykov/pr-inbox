#!/bin/zsh
# Install the same policy embedded in PR Inbox, with one shared file for local agent entry points.
set -euo pipefail
skill_home="${PR_INBOX_SKILL_HOME:-$HOME}"
skill_name=thermo-nuclear-code-quality-review
source_file="${0:A:h}/$skill_name/SKILL.md"
skill_data="$skill_home/Library/Application Support/PR Inbox"
canonical="$skill_data/skills/$skill_name/SKILL.md"
backup="$skill_data/skill-backups/$(date +%Y%m%d-%H%M%S)-$$"
/bin/mkdir -p "${canonical:h}"
/bin/cp "$source_file" "$canonical.tmp-$$"
/bin/mv -f "$canonical.tmp-$$" "$canonical"
destinations=(
  "$skill_home/.claude/skills/$skill_name/SKILL.md"
  "$skill_home/.agents/skills/$skill_name/SKILL.md"
  "$skill_home"/.agents/skills/synced/*/thermo-nuclear-code-quality-review/SKILL.md(N)
  "$skill_home"/.codex/skills/thermo-nuclear-code-quality-review/SKILL.md(N)
)
count=0
for destination in "${destinations[@]}"; do
  if [[ -L "$destination" && "$(readlink "$destination")" == "$canonical" ]]; then
    (( ++count ))
    continue
  fi
  if [[ -e "$destination" || -L "$destination" ]]; then
    saved="$backup/${destination#$skill_home/}"
    /bin/mkdir -p "${saved:h}"
    /bin/cp -pP "$destination" "$saved"
  fi
  /bin/mkdir -p "${destination:h}"
  /bin/ln -s "$canonical" "$destination.tmp-$$"
  /bin/mv -f "$destination.tmp-$$" "$destination"
  (( ++count ))
done
echo "Shared review policy installed for $count local skill entry points."
