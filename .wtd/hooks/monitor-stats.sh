#!/usr/bin/env bash
# Resource consumption attributable to the worktree-dev Claude agents — each `claude` process tree
# (the agent itself plus everything it spawns: gopls, go build, git, node …) — plus a few fleet
# counts. The diff/commit panes (less/delta) hang off the tmux shell, not claude, so they're excluded.
#
# Emits one '|'-delimited line:  sess|nag|rev|acpu|amem|mt|msys|ncpu|load
#   sess  tmux sessions                 nag   interactive claude agents (excludes -p workers)
#   rev   review agents (-p reviewer/skeptic)
#   acpu  summed %CPU of all agent trees (across cores, may exceed 100)
#   amem  summed RSS of all agent trees, MB
#   mt    total RAM MB     msys  system used RAM MB     ncpu  cores     load  1m 5m 15m
sess=$(tmux list-sessions 2>/dev/null | wc -l | tr -d ' ')
ncpu=$(nproc)
load=$(cut -d' ' -f1-3 /proc/loadavg)
read -r mt msys < <(free -m | awk '/^Mem:/{print $2, $3}')

ps -eo pid=,ppid=,pcpu=,rss=,comm=,args= | awk \
  -v sess="${sess:-0}" -v ncpu="${ncpu:-1}" -v mt="${mt:-0}" -v msys="${msys:-0}" -v loadavg="$load" '
{
  pid=$1; ppid=$2; pcpu=$3; rss=$4; comm=$5;
  PCPU[pid]=pcpu; RSS[pid]=rss; CH[ppid]=CH[ppid] " " pid;
  if (comm=="claude") {                                  # every claude proc roots an agent tree
    root[pid]=1;
    if ($0 ~ /-p/ && $0 ~ /--agent (reviewer|skeptic)/) rev++;
    else if ($0 !~ /-p( |$)/) nag++;                     # interactive agents (not -p workers)
  }
}
END {
  n=0;
  for (p in root) if (!seen[p]) { seen[p]=1; stack[++n]=p }
  while (n>0) {                                          # BFS each root + all descendants, dedup
    p=stack[n--]; cpu+=PCPU[p]+0; mem+=RSS[p]+0;
    m=split(CH[p], kids, " ");
    for (i=1;i<=m;i++) { c=kids[i]; if (c!="" && !seen[c]) { seen[c]=1; stack[++n]=c } }
  }
  printf "%s|%d|%d|%.0f|%.0f|%s|%s|%s|%s\n", sess, nag+0, rev+0, cpu+0, mem/1024, mt, msys, ncpu, loadavg
}'
