# Agent Runbook: rufa

Start the supervisor once per session with `rufa start --watch --env .env` (add `--foreground` if you want live logs in the terminal) so targets restart automatically when source changes land. Use these commands to stay informed and inspect behavior.

1. **Check Runtime State**
   - Run `rufa info` to list active targets, their PIDs, and available ports (including debug endpoints).
   - Follow the reported URLs when calling services or attaching debuggers.

2. **Review Logs**
   - Inspect the latest activity with `rufa log --tail 50`.
   - Follow streaming output for specific targets: `rufa log PRG1 --follow`.
   - Watch for `STDLOG` entries, which expose framework-specific diagnostics alongside stdout/stderr.
