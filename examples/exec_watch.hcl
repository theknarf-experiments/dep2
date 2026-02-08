# Watch running processes and filter those using significant CPU.
#
# Usage:
#   dbflow examples/exec_watch.hcl
#
# This uses `watch` to periodically run `ps` and pipes the output
# through the exec streaming plugin. Each time `watch` refreshes,
# the ANSI clear-screen triggers a snapshot diff — new processes
# are inserted, exited processes are retracted.

data "exec" "procs" {
  command = "watch -t -n2 'ps aux --no-header'"
  split   = "\\s+"
  mode    = "snapshot"
  columns = "user,pid,cpu,mem,vsz,rss,tty,stat,start,time,command"
}

resource "busy" "rule" {
  user    = data.exec.procs.user
  pid     = data.exec.procs.pid
  command = data.exec.procs.command
  _filter = data.exec.procs.cpu > 5
}

output "busy_processes" {
  value = busy.rule.pid
}
