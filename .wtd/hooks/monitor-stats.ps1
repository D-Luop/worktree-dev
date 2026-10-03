# Windows backend for monitor-stats.sh (no tmux / /proc / free here). Emits the SAME one-line,
# '|'-delimited contract the extension parses:  sess|nag|rev|acpu|amem|mt|msys|ncpu|load
#   sess  fleet session terminals (one per `agent …`/`assistant` launcher)
#   nag   interactive claude agents      rev  review workers (-p --agent reviewer/skeptic)
#   acpu  summed %CPU of every fleet claude tree (across cores, may exceed 100)
#   amem  summed working set of those trees, MB
#   mt    total RAM MB    msys  used RAM MB    ncpu  logical cores    load  n/a (no loadavg on Windows)
#
# "Fleet" claude = the npm CLI (…\npm\node_modules\@anthropic-ai\claude-code\…), NOT the bundled VSCode
# extension binary (…\.vscode\extensions\…\native-binary\claude.exe) — only the former are sessions.
#
#   -Loop <sec>  stay resident and emit one line every <sec> seconds (the extension runs it this way).
#                A cold powershell + first WMI load costs seconds; a resident one samples in ~0.3s.
#   (no -Loop)   one-shot: two samples 500ms apart (CPU% needs a delta), print once, exit.
param([int]$Loop = 0)
$ErrorActionPreference = 'SilentlyContinue'

$ncpu = [Environment]::ProcessorCount
$prevCpu = @{}; $prevAt = $null

# CPU% from per-process TotalProcessorTime deltas (Get-Process: ~40ms) instead of the
# Win32_PerfFormattedData counter class (~700ms per query).
function Sample-Cpu {
  $t = @{}
  foreach ($p in Get-Process) { try { $t[[int]$p.Id] = $p.TotalProcessorTime.TotalMilliseconds } catch {} }
  $t
}

function Emit {
  $now = [DateTime]::UtcNow
  $cpuNow = Sample-Cpu
  $cpu = @{}
  if ($prevAt) {
    $elapsed = ($now - $prevAt).TotalMilliseconds
    if ($elapsed -gt 0) {
      foreach ($k in $cpuNow.Keys) {
        if ($prevCpu.ContainsKey($k)) { $cpu[$k] = [math]::Max(0, ($cpuNow[$k] - $prevCpu[$k]) / $elapsed * 100) }
      }
    }
  }
  $script:prevCpu = $cpuNow; $script:prevAt = $now

  $procs = Get-CimInstance Win32_Process -Property ProcessId,ParentProcessId,Name,CommandLine,WorkingSetSize
  $wsOf = @{}; $children = @{}
  foreach ($p in $procs) {
    $id = [int]$p.ProcessId; $pp = [int]$p.ParentProcessId
    $wsOf[$id] = [double]$p.WorkingSetSize
    if (-not $children.ContainsKey($pp)) { $children[$pp] = New-Object 'System.Collections.Generic.List[int]' }
    $children[$pp].Add($id)
  }

  # Counts are by DISTINCT session IDENTITY parsed from the launcher (`assistant`, `agent <slug> <name>`,
  # `review <slug> <name>`), deduped in sets — so the inner/outer launcher bashes AND any duplicate/stale
  # terminal collapse to one, matching the roster (which keys by name). Raw process counts overcount.
  $agents  = New-Object 'System.Collections.Generic.HashSet[string]'
  $reviews = New-Object 'System.Collections.Generic.HashSet[string]'
  $roots   = New-Object 'System.Collections.Generic.List[int]'
  foreach ($p in $procs) {
    $cl = $p.CommandLine
    if (-not $cl) { continue }
    if ($p.Name -eq 'bash.exe') {
      if     ($cl -match '-lc\s+["'']?review\s+(.+)$')    { [void]$reviews.Add((($Matches[1] -replace '["'']','' -replace '\s+',' ').Trim())) }
      elseif ($cl -match '-lc\s+["'']?assistant\b')        { [void]$agents.Add('assistant') }
      elseif ($cl -match '-lc\s+["'']?agent\s+(.+)$') {
        $id = ($Matches[1] -replace '["'']','' -replace '\s+',' ').Trim()
        if ($id -match '\S+\s+\S+') { [void]$agents.Add($id) }   # require <slug> <name>, not `agent ls` etc.
      }
    }
    elseif ($p.Name -eq 'claude.exe' -and $cl -match 'node_modules' -and $cl -notmatch 'native-binary') {
      $roots.Add([int]$p.ProcessId)
    }
  }
  $nag = $agents.Count; $rev = $reviews.Count; $sess = $nag + $rev

  # sum each fleet claude tree (root + all descendants, deduped)
  $seen = @{}; $acpu = 0.0; $amemBytes = 0.0
  $stack = New-Object 'System.Collections.Generic.Stack[int]'
  foreach ($r in $roots) { if (-not $seen.ContainsKey($r)) { $seen[$r] = $true; $stack.Push($r) } }
  while ($stack.Count -gt 0) {
    $p = $stack.Pop()
    if ($cpu.ContainsKey($p)) { $acpu += $cpu[$p] }
    if ($wsOf.ContainsKey($p)) { $amemBytes += $wsOf[$p] }
    if ($children.ContainsKey($p)) { foreach ($c in $children[$p]) { if (-not $seen.ContainsKey($c)) { $seen[$c] = $true; $stack.Push($c) } } }
  }

  $os   = Get-CimInstance Win32_OperatingSystem -Property TotalVisibleMemorySize,FreePhysicalMemory
  $mt   = [math]::Round($os.TotalVisibleMemorySize / 1024)
  $msys = [math]::Round(($os.TotalVisibleMemorySize - $os.FreePhysicalMemory) / 1024)
  $amem = [math]::Round($amemBytes / 1MB)
  $acpuOut = [math]::Round($acpu)

  # A closed pipe (the reader died) must end a resident sampler, not leave it looping as an orphan.
  try {
    [Console]::Out.WriteLine(('{0}|{1}|{2}|{3}|{4}|{5}|{6}|{7}|{8}' -f $sess,$nag,$rev,$acpuOut,$amem,$mt,$msys,$ncpu,'n/a'))
    [Console]::Out.Flush()
  } catch { exit 0 }
}

if ($Loop -gt 0) {
  $parent = (Get-CimInstance Win32_Process -Filter "ProcessId=$PID" -Property ParentProcessId).ParentProcessId
  $script:prevCpu = Sample-Cpu; $script:prevAt = [DateTime]::UtcNow
  Start-Sleep -Milliseconds 500
  while ($true) {
    if ($parent -and -not (Get-Process -Id $parent -ErrorAction SilentlyContinue)) { exit 0 }
    Emit; Start-Sleep -Seconds $Loop
  }
} else {
  $script:prevCpu = Sample-Cpu; $script:prevAt = [DateTime]::UtcNow
  Start-Sleep -Milliseconds 500
  Emit
}
