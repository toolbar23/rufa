# rufa

rufa is a Rust command-line assistant designed to keep coding agents in sync with the software they develop. It launches developer-defined targets in the background, captures every output stream (stdout, stderr, stdlog), and exposes that information through a simple CLI so agents can close the development loop quickly.

- Keep targets running with the latest code while the agent iterates on tasks.
- Automatic port-selection allows running multiple instances of your app in parallel from different git workspaces with multiple agents working in parallel.
- Centralize logs into a single structured file (`rufa.log`) for fast inspection.
- Allow agents to query runtime state (`rufa info`), tail logs, and orchestrate restarts without manual process wrangling.
- Maintain reproducible configuration in `rufa.toml`, including port assignments, environment, and composite targets.

## Key Features
- Background supervisor with `.rufa.lock` IPC for additional commands.
- Services vs. jobs: services restart automatically, jobs run once with optional timeouts.
- Structured logging with rotation and history retention.
- Composite targets that bundle multiple services together.
- File watching for automatic restarts and backoff policies for unstable targets.
- Debug port discovery surfaced through the `info` subcommand.

## Configuration Overview
Create `rufa.toml` at the project root. Example:

```toml
[log]
file = "rufa.log"
rotation_after_seconds = 86400
rotation_after_size_mb = 10
keep_history_count = 5

[restart]
type = "BACKOFF"

[env]
read_env_file = ".env"

[target.FRONT_BACK]
type = "composite"
targets = "PRG1, PRG2"

[target.PRG1]
kind = "service"
type = "java-spring-boot"
module = "panda-core"
# Defaults to `mvn -pl panda-core spring-boot:run`
port.http.type = "http"
port.http.auto_range = "8000-8100"
port.debug.type = "tcp"
port.debug.auto = true
env.ADD_THIS_ENVVAR = "something"
env.ADD_A_REFERENCE_TO_ANOTHER_TARGETS_PORT = "{PRG2:port.http}"

[target.PRG2]
kind = "job"
timeout_seconds = 300
type = "bun"
script = "bun run report.ts"
port.metrics.type = "tcp"
port.metrics.fixed = 9100

[target.CLEANUP]
kind = "job"
type = "bash"
command = "./scripts/cleanup.sh"
```

### Target Kinds
- `service`: long-running process that should be supervised and restarted according to policy.
- `job`: one-shot task with optional `timeout_seconds`; no automatic restart.

### Target Types
Rufa ships with adapters for these target `type` values:
- `java-spring-boot` – Launch Spring Boot applications (JAR/Gradle/Maven); rufa auto-discovers debug ports from logs.
- `rust` – Build and run Cargo binaries with future support for incremental reloads.
- `bun` – Execute Bun scripts or package.json tasks (`bun run …`), ideal for TypeScript/JavaScript tooling.
- `bash` – Execute raw shell commands or scripts; perfect for one-off jobs and utility tasks.

### Ports
- Declare each port via `port.<name>.type` (`http`, `ftp`, or `tcp`).
- Choose exactly one allocation strategy:
  * `port.<name>.auto = true` – let rufa assign an available ephemeral port.
  * `port.<name>.auto_range = "START-END"` – constrain allocation to a specific inclusive range.
  * `port.<name>.fixed = PORT` – always use the given port number.

### Watch Paths
- Services restart automatically in `--watch` mode when files change.
- Add `watch = ["relative/path", "/abs/path"]` to a service target to restrict restarts to specific directories.
- When omitted, rufa watches the entire workspace.
- Control who handles restarts via `watch_type` (defaults to `PREFER_RUNTIME_SUPPLIED`). Use `RUFA` to force rufa-managed restarts; leave default to let runtime-specific flags (e.g., Bun `--watch`) be injected automatically.

- `rufa start [--watch|--no-watch] [--foreground] [--env FILE]` – launch the daemon. By default it runs in the background; add `--foreground` to keep it attached to the terminal and mirror log output. Use `--watch` to make future runs default to restart-on-change, and supply `--env` to load variables before booting the supervisor.
- `rufa run TARGET... [--watch|--no-watch] [--foreground]` – start one or more targets. When `--watch`/`--no-watch` is omitted, the daemon’s default (set via `rufa start`) applies. Add `--foreground` to stream just those targets’ logs and stop them with Ctrl+C.
- `rufa kill TARGET...` – terminate running targets without shutting down the daemon.
- `rufa info [--foreground] [--log-length N]` – show the currently running set, their PIDs, ports, debug addresses, and recent log lines (default 5). Add `--foreground` for a live, in-place view.
- `rufa log [TARGET...] [--follow] [--tail N] [--generation G] [--all]` – inspect the combined log, filtered by target or generation. Defaults to showing the latest generation for each target; pass `--all` for full history. Operates directly on `rufa.log` so you can view history even if the daemon is offline.
- `rufa restart TARGET... [--all]` – recycle services, incrementing generation counters.
- `rufa stop` – shut down the daemon and release the lockfile.

All log lines follow the format:

```
TARGET | GENERATION | STREAM | message
```

`STREAM` can be `STARTED`, `STDOUT`, `STDERR`, `STDLOG`, `STDIN`, or `EXITED`.

## Shell Completion
Generate static completions so you never mistype a flag:

```bash
# Bash
rufa completions bash > ~/.local/share/bash-completion/rufa
source ~/.local/share/bash-completion/rufa

# Zsh
rufa completions zsh > "${ZDOTDIR:-$HOME}/.zsh/completions/_rufa"
autoload -U compinit && compinit

# Fish
rufa completions fish > ~/.config/fish/completions/rufa.fish
```

Re-run the command after upgrading rufa so the completions stay current. Command names and flags are covered; target/task lists remain manual for now.

## Agent-Oriented Workflow
1. Configure targets in `rufa.toml` (services and jobs).
2. Start the supervisor with `rufa start` (add `--background` to detach).
3. Launch the desired targets via `rufa run ...`.
4. Use `rufa info` to share runtime details (ports, URLs, debug ports) with the agent.
5. The agent reads `rufa log` or tails specific targets to validate behavior after each change; use `rufa run --watch ...` (or configure the default via `rufa start --watch`) for automatic restarts.
6. Use `rufa kill` to stop individual targets or `rufa stop` when the development session ends to clean up.

## Notes for Future Enhancements
- Import launch definitions from VS Code `launch.json`.
- Windows named-pipe IPC and service control.
- Richer agent hooks for scripted workflows (e.g., JSON API).

For agent-facing instructions, see `example/AGENTS.md`.
