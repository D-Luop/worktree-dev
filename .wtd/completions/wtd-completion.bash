# Bash tab-completion for the worktree-dev commands (agent / archive / review).
# Installed to ~/.local/share/bash-completion/completions/{agent,archive,review} (symlinks to this
# file); bash-completion sources it lazily on first <cmd><TAB>.
#
# Completes: repo slugs, agent subcommands, and — the point — branch/worktree NAMES for a slug, so
# `agent <slug> feat/<TAB>` offers feat/contract-change-status, feat/job-setup, …

# existing worktree NAMES for a slug (for stop/rm/archive/review — they need a live worktree).
# Skips the reserved archive/ namespace (those are archived, not active).
_wtd_worktrees() {
  local slug="$1" dev="$HOME/dev" d
  for d in "$dev"/worktrees/"$slug"/*/ "$dev"/worktrees/"$slug"/*/*/; do
    case "$d" in "$dev"/worktrees/"$slug"/archive/*) continue;; esac
    [ -e "$d/.git" ] && { d="${d#"$dev/worktrees/$slug/"}"; printf '%s\n' "${d%/}"; }  # .git ⇒ real worktree, not a namespace dir
  done 2>/dev/null | sort -u
}
# branches (local + origin, minus HEAD) + existing worktrees (for `agent <slug> <name>`: open any branch)
_wtd_names() {
  local slug="$1" bare="$HOME/dev/repos/$1/.bare"
  {
    git -c safe.bareRepository=all -C "$bare" for-each-ref --format='%(refname:short)' refs/heads 2>/dev/null
    git -c safe.bareRepository=all -C "$bare" for-each-ref --format='%(refname:lstrip=3)' refs/remotes/origin 2>/dev/null | grep -vx HEAD
    _wtd_worktrees "$slug"
    _wtd_archived "$slug"
  } 2>/dev/null | sort -u
}
# archived worktree NAMES for a slug (worktrees/<slug>/archive/<name>) — so `agent <slug> <TAB>`
# also offers archived branches you can reopen (agent prompts y/n to restore them).
_wtd_archived() {
  local slug="$1" dev="$HOME/dev" d
  for d in "$dev"/worktrees/"$slug"/archive/*/ "$dev"/worktrees/"$slug"/archive/*/*/; do
    [ -e "$d/.git" ] && { d="${d#"$dev/worktrees/$slug/archive/"}"; printf '%s\n' "${d%/}"; }
  done 2>/dev/null | sort -u
}

# named Claude accounts (~/.claude-accounts/<name>) for `agent --account` / `account login|rm`
_wtd_accounts() {
  local d
  for d in "$HOME"/.claude-accounts/*/; do [ -d "$d" ] && basename "$d"; done 2>/dev/null | sort -u
}

_wtd_complete() {
  local reg="$HOME/dev/.wtd/repos.tsv"
  local cmd="${COMP_WORDS[0]##*/}" cur="${COMP_WORDS[COMP_CWORD]}" cw="$COMP_CWORD"
  local prev="${COMP_WORDS[COMP_CWORD-1]:-}"
  local slugs; slugs=$(awk -F'\t' '!/^#/ && $1!=""{print $1}' "$reg" 2>/dev/null)

  # --account <TAB> on any command → account names
  case "$prev" in --account|-a) COMPREPLY=( $(compgen -W "$(_wtd_accounts)" -- "$cur") ); return;; esac

  case "$cmd" in
    agent)
      if [ "$cw" -eq 1 ]; then
        COMPREPLY=( $(compgen -W "$slugs ls stop done wip rm" -- "$cur") ); return
      fi
      case "${COMP_WORDS[1]}" in
        rm|stop|kill)
          [ "$cw" -eq 2 ] && COMPREPLY=( $(compgen -W "$slugs" -- "$cur") )
          [ "$cw" -eq 3 ] && COMPREPLY=( $(compgen -W "$(_wtd_worktrees "${COMP_WORDS[2]}")" -- "$cur") );;
        *)
          [ "$cw" -eq 2 ] && COMPREPLY=( $(compgen -W "$(_wtd_names "${COMP_WORDS[1]}")" -- "$cur") )
          [ "$cw" -ge 3 ] && COMPREPLY=( $(compgen -W "--account --from --no-claude" -- "$cur") );;
      esac ;;
    archive|review)
      [ "$cw" -eq 1 ] && COMPREPLY=( $(compgen -W "$slugs" -- "$cur") )
      [ "$cw" -eq 2 ] && COMPREPLY=( $(compgen -W "$(_wtd_worktrees "${COMP_WORDS[1]}")" -- "$cur") ) ;;
    ask)
      [ "$cw" -eq 1 ] && COMPREPLY=( $(compgen -W "$slugs" -- "$cur") ) ;;
    account)
      [ "$cw" -eq 1 ] && COMPREPLY=( $(compgen -W "ls add login rm use usage" -- "$cur") )
      if [ "$cw" -eq 2 ]; then case "${COMP_WORDS[1]}" in
        login|rm)  COMPREPLY=( $(compgen -W "$(_wtd_accounts)" -- "$cur") );;
        usage)     COMPREPLY=( $(compgen -W "default $(_wtd_accounts)" -- "$cur") );;
        use)       COMPREPLY=( $(compgen -W "dev review" -- "$cur") );;
      esac; fi
      [ "$cw" -eq 3 ] && [ "${COMP_WORDS[1]}" = use ] && COMPREPLY=( $(compgen -W "default $(_wtd_accounts)" -- "$cur") ) ;;
  esac
}
complete -F _wtd_complete agent archive review ask account
