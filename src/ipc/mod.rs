mod lock;
mod protocol;

pub use lock::{LockInfo, runtime_dir};
pub use protocol::{
    AvailableTargetSummary, BehaviorKind, ConfigureRequest, ControlStateSummary, ExitDetails,
    InfoRequest, InfoResponse, KillRequest, LogRequest, PortSummary, RestartRequest,
    RunHistorySummary, RunRequest, RunningTargetSummary, ServerResponse, StoppedTargetSummary,
    TargetConfigSummary, TargetRunState, WatchPreferenceKind,
};

#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use unix::{
    connect_existing_client, require_daemon_client, run_daemon, spawn_daemon_process,
    wait_for_daemon_ready, wait_for_shutdown,
};

#[cfg(not(unix))]
mod unix {
    use std::time::Duration;

    use anyhow::{Result, bail};

    use super::protocol::ClientCommand;
    use super::{InfoRequest, LogRequest, RestartRequest, RunRequest, ServerResponse};

    #[derive(Debug, Clone)]
    pub struct IpcClient;

    pub async fn ensure_daemon_running() -> Result<IpcClient> {
        bail!("IPC is not supported on this platform")
    }

    pub async fn require_daemon_client() -> Result<IpcClient> {
        bail!("IPC is not supported on this platform")
    }

    pub async fn connect_existing_client() -> Result<Option<IpcClient>> {
        Ok(None)
    }

    pub async fn run_daemon() -> Result<()> {
        bail!("IPC is not supported on this platform")
    }

    pub async fn run_daemon_foreground(_request: RunRequest) -> Result<()> {
        bail!("IPC is not supported on this platform")
    }

    pub async fn spawn_daemon_process(
        _default_watch: bool,
        _env_file: Option<&std::path::Path>,
    ) -> Result<()> {
        bail!("IPC is not supported on this platform")
    }

    pub async fn wait_for_daemon_ready(_timeout: Duration) -> Result<IpcClient> {
        bail!("IPC is not supported on this platform")
    }

    pub async fn wait_for_shutdown(_timeout: Duration) -> Result<()> {
        bail!("IPC is not supported on this platform")
    }

    impl IpcClient {
        pub async fn send(&self, _command: ClientCommand) -> Result<ServerResponse> {
            bail!("IPC is not supported on this platform")
        }

        pub async fn run(&self, _request: RunRequest) -> Result<ServerResponse> {
            bail!("IPC is not supported on this platform")
        }

        pub async fn info(&self, _request: InfoRequest) -> Result<ServerResponse> {
            bail!("IPC is not supported on this platform")
        }

        pub async fn log(&self, _request: LogRequest) -> Result<ServerResponse> {
            bail!("IPC is not supported on this platform")
        }

        pub async fn restart(&self, _request: RestartRequest) -> Result<ServerResponse> {
            bail!("IPC is not supported on this platform")
        }

        pub async fn stop(&self) -> Result<ServerResponse> {
            bail!("IPC is not supported on this platform")
        }
    }
}
