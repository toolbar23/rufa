# RUFA Development Plan

## 1. Project Setup
- Initialize a new Cargo binary crate named `rufa`.
- Configure dependencies: `clap` (derive + command + completions), `tokio`, `serde` + `serde_json`, `toml`, `notify`, `thiserror`, `anyhow`, `tracing`, and platform-specific crates for IPC (Unix sockets, Windows named pipes).
- Establish workspace layout (`src/` modules), place sample `rufa.toml`, `.env`, and fixture targets for testing.

## 2. Configuration Layer
- Define strongly typed structures mirroring sections: `[log]`, `[env]`, and `[target.*]`.
- Support target classification into `service` (long-running, restartable) and `job` (one-shot, optional timeout).
- Support composite targets (`type = "composite"`) by recursively expanding `targets`.
- Allow `job` targets to specify execution timeout and failure policy in configuration.
- Implement environment interpolation, including references to other targets’ exported data (`{PRG2:port.http}`).
- Load optional `.env` file and merge with per-target environment, providing override precedence rules.
- Stub VS Code `launch.json` importer behind a trait to allow future integration.

## 3. Runtime State & IPC
- Create `.rufa.lock` containing daemon PID and IPC endpoint.
- Use Unix domain socket (later Windows pipe) for command/control messages.
- Define protocol enums (`Run`, `Info`, `Log`, `Restart`, `Stop`) and serialization (e.g., `bincode` or `serde_json`).
- Maintain in-memory state tracking generations, PIDs, port assignments, last log lines, and debug-port discoveries.

## 4. Process Management
- Resolve selected targets, expand composites, allocate required ports (support ranges with reservation tracking).
- Prepare environment variables and command invocations per target type (`java-spring-boot`, etc.).
- Spawn processes via `tokio::process::Command`, piping `stdout`, `stderr`, and an additional logical `STDLOG` stream when available.
- Apply restart supervision only to `service` targets; record generation lifecycle separately from `job` executions.
- Enforce `job`-specific timeouts and mark completion status without automatic restart.
- Detect dynamic debug ports by parsing log output with configurable patterns; record findings in state.

## 5. Logging & Rotation
- Implement centralized async logger that receives structured events: `STARTED`, `STDOUT`, `STDERR`, `STDLOG`, `STDIN`, `EXITED`.
- Enforce fixed-width fields (`TARGET`, `GEN`, `EVENT`) and write to `rufa.log`.
- Support rotation policies: age (`rotation_after_seconds`), size (`rotation_after_size_mb`), and retention (`keep_history_count`).
- Ensure rotated archives are preserved (`rufa.log.1` …) and concurrent readers can gracefully handle rotation.

## 6. Command-Line Interface
- `target start`: launch targets (respecting the daemon's watch default) and honor job timeouts without restart.
- `info`: query daemon, display PID, port mappings (including URLs), last log lines, and debug-port info per target.
- `log`: stream combined log file or filtered view; implement `--follow`, `--tail`, `--generation`, target filters.
- `target restart`: restart specific service targets or all (`--all`), respecting graceful shutdown and generation incrementing; reject job restart unless explicitly re-run.
- `stop`: terminate all running targets, flush logs, clean up lock/IPC artifacts.
- Ship shell completion installers (bash/zsh/fish/powershell) leveraging Clap’s completion generation and provide `rufa completions <shell>` subcommand.

## 7. Watch & Restart Logic
- Integrate `notify` to watch source paths; map file events to owning targets.
- Queue restart requests with debounce to avoid thrashing.
- Ensure generations increment per restart for services, while jobs ignore watch-triggered restarts unless explicitly re-run.

## 8. Testing & Tooling
- Unit tests for configuration parsing (composites, env interpolation, port allocation).
- Integration tests using fixture scripts to mimic targets, verifying logging, rotation, and restart flows.
- End-to-end tests for CLI subcommands via `assert_cmd`, including background daemon handshake.
- Provide developer tooling tasks (e.g., `cargo make` or scripts) for linting, formatting, and smoke runs.

## 9. Documentation
- Document configuration schema, default behaviors, and target type contracts.
- Provide examples for composite targets, watchdog restarts, and debug port detection.
- Outline future work: VS Code launch import, Windows IPC support, richer runtime inspection.

## Milestones
1. [x] **Scaffold Project**
   - `cargo init --bin rufa`, configure dependencies, set up module skeletons (`config`, `ipc`, `runner`, `logging`, `cli`, `watch`, `state`).
   - Add baseline CI workflow (lint/format/test) and placeholder `rufa.toml` example.
2. [x] **Configuration Parsing**
   - Implement TOML deserialization, composite expansion, env interpolation, service/job distinction, and timeout validation.
   - Load `.env` and prepare unit tests covering edge cases (port ranges, env references, composites).
3. [x] **IPC & Daemon Skeleton**
   - Create `.rufa.lock` management, Unix socket server, and request/response protocol scaffolding.
   - Implement CLI detection of active daemon and command forwarding.
4. [x] **Process Runner & Logging**
   - Launch targets with `tokio::process`, capture stdout/stderr/stdlog, push events through structured logger.
   - Implement log formatting, rotation policies, and persistence of runtime state.
5. [x] **CLI Command Implementation**
   - Flesh out `start`, `target start`, `info`, `log`, `target restart`, `stop` subcommands; integrate shell completion generation command.
   - Ensure log streaming, filtering, and generation tracking function via IPC.
6. [x] **Watch & Restart Flow**
   - Connect file watcher to restart queue, apply debounce, respect service/job semantics and timeouts.
   - Extract debug port info from logs and surface via `info`.
7. [ ] **Quality & Documentation**
   - Expand integration tests and add example fixtures; finalize README and agent documentation.
   - Polish error messages, telemetry hooks, and prepare release notes for initial version.
