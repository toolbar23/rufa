use anyhow::{Context, Result, bail};
use std::{
    collections::HashSet,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::TcpListener as TokioTcpListener,
    sync::{RwLock, mpsc},
    task::JoinHandle,
    time::sleep,
};

use crate::config::model::PortSelection;

#[derive(Debug, Default)]
pub(crate) struct PortAllocator {
    reserved: HashSet<u16>,
}

impl PortAllocator {
    pub(crate) fn reserve_specific(&mut self, port: u16) -> Result<()> {
        if self.reserved.contains(&port) {
            bail!("port {port} is already reserved");
        }

        TcpListener::bind(("127.0.0.1", port))
            .context(format!("binding to specific port {port}"))?;
        self.reserved.insert(port);
        Ok(())
    }

    pub(crate) fn allocate(&mut self, selection: PortSelection) -> Result<u16> {
        match selection {
            PortSelection::Auto => self.allocate_ephemeral(),
            PortSelection::AutoRange { start, end } => self.allocate_from_range(start, end),
            PortSelection::Fixed(port) => {
                self.allocate_fixed(port)?;
                Ok(port)
            }
        }
    }

    pub(crate) fn release_ports<I>(&mut self, ports: I)
    where
        I: Iterator<Item = u16>,
    {
        for port in ports {
            self.reserved.remove(&port);
        }
    }

    fn allocate_ephemeral(&mut self) -> Result<u16> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .context("binding to ephemeral port for allocation")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        self.reserved.insert(port);
        Ok(port)
    }

    fn allocate_from_range(&mut self, start: u16, end: u16) -> Result<u16> {
        for port in start..=end {
            if self.reserved.contains(&port) {
                continue;
            }
            if self.reserve_specific(port).is_ok() {
                return Ok(port);
            }
        }
        bail!("no free ports in range {start}-{end}");
    }

    fn allocate_fixed(&mut self, port: u16) -> Result<()> {
        self.reserve_specific(port)
    }
}

#[derive(Debug)]
pub(crate) struct PortSentrySet {
    sentries: Vec<PortSentry>,
}

impl PortSentrySet {
    pub(crate) async fn bind_from_ports(
        ports: &std::collections::HashMap<String, u16>,
    ) -> io::Result<Self> {
        let mut seen = HashSet::new();
        let mut sentries = Vec::new();
        for port in ports.values().copied() {
            if seen.insert(port) {
                sentries.push(PortSentry::bind(port).await?);
            }
        }
        Ok(Self { sentries })
    }

    pub(crate) async fn stop(self) {
        for sentry in self.sentries {
            sentry.stop().await;
        }
    }
}

#[derive(Debug)]
struct PortSentry {
    port: u16,
    shutdown: tokio::sync::watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl PortSentry {
    async fn bind(port: u16) -> io::Result<Self> {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
        let listener = TokioTcpListener::bind(addr).await?;
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            let mut shutdown_rx_inner = shutdown_rx.clone();
            loop {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_ok() && *shutdown_rx.borrow() {
                            break;
                        }
                        let _ = shutdown_rx.borrow_and_update();
                    }
                    result = listener.accept() => {
                        match result {
                            Ok((stream, _)) => {
                                let hold = sleep(Duration::from_millis(100));
                                tokio::pin!(hold);
                                tokio::select! {
                                    _ = &mut hold => {}
                                    changed = shutdown_rx_inner.changed() => {
                                        if changed.is_ok() && *shutdown_rx_inner.borrow() {
                                            return;
                                        }
                                        let _ = shutdown_rx_inner.borrow_and_update();
                                        continue;
                                    }
                                }
                                drop(stream);
                            }
                            Err(_) => {
                                // Ignore transient accept errors while holding the port.
                                let _ = sleep(Duration::from_millis(10)).await;
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            port,
            shutdown: shutdown_tx,
            handle,
        })
    }

    async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.handle.await;
    }
}

pub(crate) async fn restore_port_sentries(
    store: Arc<RwLock<std::collections::HashMap<String, PortSentrySet>>>,
    target: String,
    ports: std::collections::HashMap<String, u16>,
) -> Result<()> {
    let sentries = PortSentrySet::bind_from_ports(&ports)
        .await
        .with_context(|| format!("binding placeholder listeners for target {}", target))?;
    let previous = {
        let mut map = store.write().await;
        map.insert(target.clone(), sentries)
    };
    if let Some(prev) = previous {
        prev.stop().await;
    }
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn release_ports_from_assignments(
    allocator: &mut PortAllocator,
    ports: std::collections::HashMap<String, u16>,
) {
    allocator.release_ports(ports.into_values());
}

pub(crate) fn channel_pair<T>() -> (mpsc::Sender<T>, mpsc::Receiver<T>) {
    mpsc::channel(32)
}
