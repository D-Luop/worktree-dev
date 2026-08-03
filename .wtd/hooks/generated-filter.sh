#!/usr/bin/env bash
# Shared helper for the diff viewers: decide which files are GENERATED so they can be
# excluded from a diff. Repo-agnostic — it asks the repo, no hardcoded module paths:
#   1) the repo's own .gitattributes marks the file  linguist-generated=true, OR
#   2) the file's first lines carry a generator marker
#      ("Code generated … DO NOT EDIT." for Go, "@generated" for protobuf-ts etc.)
# Sourced (not executed) by commit-diff-show.sh and repo-diff.sh.

# gen_excludes <root> <ref|->
#   Reads newline-separated repo-relative paths on stdin; writes a git ':(exclude)<path>'
#   pathspec (one per line) for each path that is generated.
#   ref "-" reads file content from the working tree; otherwise from that git ref (e.g. a SHA).

# test_excludes — when the user has toggled "exclude test files" in the panel (the flag file exists),
# emit git ':(exclude)<glob>' pathspecs for test files so the diff viewers drop them; else nothing.
# Repo-agnostic globs (Go/proto/sql/ts). Flag path overridable via WTD_TESTS_FLAG.
test_excludes() {
  [ -f "${WTD_TESTS_FLAG:-$HOME/.config/wtd/exclude-tests}" ] || return 0
  printf ':(exclude)%s\n' \
    '**/*_test.go' '**/*_test.sql' '**/*_test.ts' '**/*.test.ts' '**/*.spec.ts' '**/testdata/**'
  return 0
}

gen_excludes() {
  local root="$1" ref="$2" line val path head
  local -a files=()
  while IFS= read -r path; do [ -n "$path" ] && files+=("$path"); done
  [ "${#files[@]}" -gt 0 ] || return 0

  # 1) .gitattributes linguist-generated (one batched call). Output line:
  #    "<path>: linguist-generated: true|false|unspecified"
  declare -A is_gen=()
  while IFS= read -r line; do
    val="${line##*: }"; path="${line%%: linguist-generated:*}"
    [ "$val" = "true" ] && is_gen["$path"]=1
  done < <(git -C "$root" check-attr linguist-generated -- "${files[@]}" 2>/dev/null)

  # 2) content-marker fallback for anything not already flagged by attributes
  for path in "${files[@]}"; do
    if [ -z "${is_gen[$path]:-}" ]; then
      if [ "$ref" = "-" ]; then head="$(head -5 "$root/$path" 2>/dev/null)"
      else                      head="$(git -C "$root" show "$ref:$path" 2>/dev/null | head -5)"; fi
      printf '%s' "$head" | grep -qE 'Code generated .* DO NOT EDIT|@generated' && is_gen["$path"]=1
    fi
    [ -n "${is_gen[$path]:-}" ] && printf ':(exclude)%s\n' "$path"
  done
}
