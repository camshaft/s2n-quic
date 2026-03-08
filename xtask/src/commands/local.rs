// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    process::Stdio,
    sync::Arc,
};
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    signal,
    sync::{self, mpsc},
    task::JoinHandle,
};
use xshell::{cmd, Shell};

#[derive(Args)]
pub struct Local {
    /// Number of RPC server nodes to start
    #[arg(long, default_value = "1")]
    servers: usize,

    /// Number of RPC client nodes to start
    #[arg(long, default_value = "1")]
    clients: usize,

    /// Cargo build profile
    #[arg(default_value = "dev", long)]
    profile: String,

    /// Log level (env: S2N_LOG)
    #[arg(long, env = "S2N_LOG")]
    log_level: Option<String>,

    /// Path to configuration file specifying remote hosts
    #[arg(long, short)]
    config: Option<PathBuf>,

    /// Path to dc-tester config file
    #[arg(long)]
    dc_config: Option<PathBuf>,

    /// Directory for diagnostic event traces (one JSON file per errored stream)
    #[arg(long, default_value = "/tmp/dc-traces")]
    trace_dir: PathBuf,
}

impl Local {
    pub fn run(self, sh: &Shell) -> Result<()> {
        let mut nodes = Nodes::from_config(sh, &self.config)?;
        let binary_dir = self.binary_dir()?;

        eprintln!("Setting up nodes...");
        std::thread::scope(|s| {
            let mut handles = vec![];
            for node in nodes.iter_mut() {
                let sh = sh.clone();
                handles.push(s.spawn(move || node.setup(&sh)));
            }
            for h in handles {
                h.join().unwrap()?;
            }
            <Result<()>>::Ok(())
        })?;

        eprintln!("Building code...");
        std::thread::scope(|s| {
            let mut handles = vec![];
            for node in nodes.iter_mut() {
                let sh = sh.clone();
                let profile = &self.profile;
                handles.push(s.spawn(move || {
                    node.deploy(&sh)?;
                    node.build(&sh, profile)?;
                    <Result<()>>::Ok(())
                }));
            }
            for h in handles {
                h.join().unwrap()?;
            }
            <Result<()>>::Ok(())
        })?;

        let mut base_port = ports();
        let mut colors = colors();
        let mut processes = Vec::new();
        let mut server_addresses = Vec::new();

        let dc_config = self
            .dc_config
            .unwrap_or_else(|| PathBuf::from("tools/dc-tester/etc/config.example.toml"));

        // Start servers
        for i in 0..self.servers {
            let node = &nodes[i % nodes.len()];
            let node_base = base_port.next().unwrap();
            let color = colors.next().unwrap();

            let server_port = node_base;
            let server_addr = SocketAddr::new(node.ip(), server_port);
            server_addresses.push(server_addr);

            let label = format!("server:{i}");

            let mut env_vars = HashMap::new();
            if let Some(ref log) = self.log_level {
                env_vars.insert("S2N_LOG".to_string(), log.to_string());
            }

            let (binary, config_path) =
                node.resolve_paths(sh, &binary_dir.join("dc-tester"), &dc_config);

            let trace_dir = self.trace_dir.display().to_string();
            let args = vec![
                "--trace-dir".to_string(),
                trace_dir,
                "server".to_string(),
                "--config".to_string(),
                config_path,
            ];

            processes.push(ProcessConfig {
                target: node.clone(),
                label,
                binary,
                args,
                env_vars,
                color,
            });
        }

        // Start clients
        for i in 0..self.clients {
            let node = &nodes[i % nodes.len()];
            let _node_base = base_port.next().unwrap();
            let color = colors.next().unwrap();

            let label = format!("client:{i}");

            let mut env_vars = HashMap::new();
            if let Some(ref log) = self.log_level {
                env_vars.insert("S2N_LOG".to_string(), log.to_string());
            }

            let (binary, config_path) =
                node.resolve_paths(sh, &binary_dir.join("dc-tester"), &dc_config);

            // Round-robin assign clients to servers
            let server_addr = server_addresses[i % server_addresses.len()];

            let trace_dir = self.trace_dir.display().to_string();
            let args = vec![
                "--trace-dir".to_string(),
                trace_dir,
                "client".to_string(),
                "--config".to_string(),
                config_path,
                "--server-addr".to_string(),
                server_addr.to_string(),
            ];

            processes.push(ProcessConfig {
                target: node.clone(),
                label,
                binary,
                args,
                env_vars,
                color,
            });
        }

        eprintln!("Starting processes...\n");
        run_processes(sh.clone(), processes)
    }

    fn binary_dir(&self) -> Result<PathBuf> {
        let profile = &self.profile;
        let dir = if profile == "dev" { "debug" } else { profile };
        // dc-tester is in its own workspace under tools/, so the binary
        // goes to tools/dc-tester/target/<profile>/dc-tester
        Ok(PathBuf::from("tools/dc-tester/target").join(dir))
    }
}

#[derive(Clone, Debug)]
enum Node {
    Local,
    Remote(RemoteNode),
}

impl Node {
    fn setup(&mut self, sh: &Shell) -> Result<()> {
        match self {
            Self::Local => Ok(()),
            Self::Remote(node) => node.setup(sh),
        }
    }

    fn deploy(&self, sh: &Shell) -> Result<()> {
        match self {
            Self::Local => Ok(()),
            Self::Remote(node) => node.deploy(sh),
        }
    }

    fn build(&self, sh: &Shell, profile: &str) -> Result<()> {
        match self {
            Self::Local => {
                cmd!(
                    sh,
                    "cargo build --manifest-path tools/dc-tester/Cargo.toml --profile {profile}"
                )
                .run()
                .context("Failed to build dc-tester")?;
                Ok(())
            }
            Self::Remote(node) => node.build(sh, profile),
        }
    }

    fn ip(&self) -> IpAddr {
        match self {
            Self::Local => std::net::Ipv6Addr::LOCALHOST.into(),
            Self::Remote(node) => node.ip.unwrap(),
        }
    }

    /// Returns (binary_path, config_path) appropriate for local or remote execution
    fn resolve_paths(
        &self,
        sh: &Shell,
        local_binary: &std::path::Path,
        local_config: &std::path::Path,
    ) -> (PathBuf, String) {
        match self {
            Self::Local => (
                sh.current_dir().join(local_binary),
                sh.current_dir().join(local_config).display().to_string(),
            ),
            Self::Remote(remote) => {
                // On remote, binary and config are relative to remote_dir
                (
                    remote.dir.join(local_binary),
                    remote.dir.join(local_config).display().to_string(),
                )
            }
        }
    }
}

#[derive(Clone, Debug)]
struct RemoteNode {
    user: Option<String>,
    host: String,
    dir: PathBuf,
    ip: Option<IpAddr>,
}

impl RemoteNode {
    fn ssh_target(&self) -> String {
        if let Some(user) = &self.user {
            format!("{}@{}", user, self.host)
        } else {
            self.host.clone()
        }
    }

    fn setup(&mut self, sh: &Shell) -> Result<()> {
        let target = self.ssh_target();
        let dir = self.dir.as_path();

        let mut script = String::new();
        let max_socket_buf = 200_000_000;
        for line in [
            &format!("mkdir -p {}", dir.display()),
            &format!("sudo sysctl -w net.core.wmem_max={max_socket_buf}"),
            &format!("sudo sysctl -w net.core.rmem_max={max_socket_buf}"),
            "sudo sysctl -w net.core.netdev_budget=600",
        ] {
            script.push_str(line);
            script.push('\n');
        }

        cmd!(sh, "ssh {target} {script}")
            .quiet()
            .run()
            .context("Failed to set up remote node")?;

        // Resolve the remote_dir (expand ~ to actual home dir)
        let dir_str = dir.display().to_string();
        if dir_str.starts_with("~/") {
            let home = cmd!(sh, "ssh {target} echo $HOME")
                .quiet()
                .read()
                .context("Failed to resolve remote HOME")?;
            let home = home.trim();
            self.dir = PathBuf::from(format!("{}/{}", home, &dir_str[2..]));
        }

        // Resolve IP
        let output = cmd!(sh, "ssh {target} hostname -I")
            .quiet()
            .read()
            .context("Failed to resolve node IP address")?;

        let mut ips: Vec<IpAddr> = output
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .filter(|ip: &IpAddr| !(ip.is_loopback() || ip.is_unspecified()))
            .collect();

        // Prefer IPv6
        ips.sort_by(|a, b| match (a, b) {
            (IpAddr::V6(_), IpAddr::V4(_)) => std::cmp::Ordering::Greater,
            (IpAddr::V4(_), IpAddr::V6(_)) => std::cmp::Ordering::Less,
            _ => a.cmp(b),
        });

        let ip = *ips.first().unwrap();
        self.ip = Some(ip);
        eprintln!("  {} -> {}", target, ip);

        Ok(())
    }

    fn deploy(&self, sh: &Shell) -> Result<()> {
        let workspace_root = sh.current_dir();
        let node = self.ssh_target();
        let remote_dir = self.dir.as_path();

        let src = format!("{}/", workspace_root.display());
        let dest = format!("{}:{}/", node, remote_dir.display());

        let rsync_args = ["--exclude=target/*", "--exclude=.git/", "-avz", &src, &dest];

        cmd!(sh, "rsync {rsync_args...}")
            .quiet()
            .run()
            .context("Failed to rsync code to remote node")?;

        Ok(())
    }

    fn build(&self, sh: &Shell, profile: &str) -> Result<()> {
        let node = self.ssh_target();
        let remote_dir = self.dir.as_path();

        let build_cmd = format!(
            "cd {} && cargo build --manifest-path tools/dc-tester/Cargo.toml --profile {profile}",
            remote_dir.display()
        );

        let shell_cmd = format!("bash --login -c {build_cmd:?}");

        cmd!(sh, "ssh {node} {shell_cmd}")
            .quiet()
            .run()
            .context("Failed to build on remote node")?;

        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct LocalConfig {
    #[serde(default)]
    host: Vec<HostConfig>,
    #[serde(default = "LocalConfig::default_remote_dir")]
    remote_dir: PathBuf,
}

impl LocalConfig {
    fn default_remote_dir() -> PathBuf {
        PathBuf::from("~/s2n-quic")
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct HostConfig {
    hostname: String,
    #[serde(default)]
    user: Option<String>,
}

struct Nodes {
    nodes: Vec<Node>,
}

impl Nodes {
    fn from_config(sh: &Shell, config_path: &Option<PathBuf>) -> Result<Self> {
        if let Some(path) = config_path {
            let content = sh
                .read_file(path)
                .with_context(|| format!("Failed to read config file: {}", path.display()))?;
            let cfg: LocalConfig = toml::from_str(&content)
                .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

            let mut nodes: Vec<Node> = cfg
                .host
                .iter()
                .map(|host| {
                    Node::Remote(RemoteNode {
                        user: host.user.clone(),
                        host: host.hostname.clone(),
                        dir: cfg.remote_dir.clone(),
                        ip: None,
                    })
                })
                .collect();

            if nodes.is_empty() {
                nodes.push(Node::Local);
            }

            Ok(Self { nodes })
        } else {
            Ok(Self {
                nodes: vec![Node::Local],
            })
        }
    }

    fn iter_mut(&mut self) -> std::slice::IterMut<'_, Node> {
        self.nodes.iter_mut()
    }

    fn len(&self) -> usize {
        self.nodes.len()
    }
}

impl std::ops::Index<usize> for Nodes {
    type Output = Node;
    fn index(&self, index: usize) -> &Self::Output {
        &self.nodes[index]
    }
}

struct ProcessConfig {
    target: Node,
    label: String,
    binary: PathBuf,
    args: Vec<String>,
    env_vars: HashMap<String, String>,
    color: Color,
}

fn colors() -> impl Iterator<Item = Color> {
    [
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
    ]
    .into_iter()
    .cycle()
}

fn ports() -> impl Iterator<Item = u16> {
    (0..).map(|v| 4433 + v * 100)
}

#[tokio::main]
async fn run_processes(_sh: Shell, configs: Vec<ProcessConfig>) -> Result<()> {
    let max_label_len = configs.iter().map(|c| c.label.len()).max().unwrap_or(0);

    let mut children = Vec::new();
    let (tx, mut rx) = mpsc::channel(512);

    for config in &configs {
        let child = spawn_process(config, tx.clone()).await?;
        children.push((config.label.clone(), child));
    }

    drop(tx);

    let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())
        .context("Failed to set up SIGINT handler")?;
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
        .context("Failed to set up SIGTERM handler")?;

    let mut stdout = StandardStream::stdout(ColorChoice::Auto);

    let (shutdown, mut exit_task) = monitor_processes(children);

    loop {
        tokio::select! {
            Some((color, label, line)) = rx.recv() => {
                print_line(&mut stdout, color, &label, &line, max_label_len)?;
            }
            _ = sigint.recv() => {
                eprintln!("\nReceived Ctrl+C, shutting down...\n");
                break;
            }
            _ = sigterm.recv() => {
                eprintln!("\nReceived SIGTERM, shutting down...\n");
                break;
            }
            _ = &mut exit_task => {
                eprintln!("\nChild processes exited, shutting down...\n");
                break;
            }
        }
    }

    drop(shutdown);

    if !exit_task.is_finished() {
        let _ = exit_task.await;
    }

    Ok(())
}

async fn spawn_process(
    config: &ProcessConfig,
    tx: mpsc::Sender<(Color, Arc<str>, String)>,
) -> Result<Child> {
    let mut child = match &config.target {
        Node::Local => {
            let mut cmd = Command::new(&config.binary);
            cmd.args(&config.args)
                .envs(&config.env_vars)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);

            cmd.spawn()
                .with_context(|| format!("Failed to spawn process: {:?}", config.binary))?
        }
        Node::Remote(remote) => {
            let mut cmd = Command::new("ssh");
            cmd.arg("-tt").arg(remote.ssh_target());

            let mut remote_cmd = String::new();
            remote_cmd.push_str("stty -opost; ");
            for (key, value) in &config.env_vars {
                remote_cmd.push_str(&format!("export {}='{}'; ", key, value));
            }
            remote_cmd.push_str(&format!("cd {}; ", remote.dir.display()));
            remote_cmd.push_str(&format!("{}", config.binary.display()));
            for arg in &config.args {
                remote_cmd.push_str(&format!(" '{}'", arg));
            }

            cmd.arg(remote_cmd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);

            cmd.spawn()
                .with_context(|| format!("Failed to spawn remote process on {}", remote.host))?
        }
    };

    let stdout = child.stdout.take().context("Failed to capture stdout")?;

    let label: Arc<str> = config.label.clone().into();
    let color = config.color;

    let label_clone = label.clone();
    let tx_clone = tx.clone();
    tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = tx_clone.send((color, label_clone.clone(), line)).await;
        }
    });

    if let Some(stderr) = child.stderr.take() {
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx_clone.send((color, label.clone(), line)).await;
            }
        });
    }

    Ok(child)
}

fn print_line(
    stdout: &mut StandardStream,
    color: Color,
    label: &str,
    line: &str,
    max_label_len: usize,
) -> Result<()> {
    use std::io::Write;

    stdout.set_color(ColorSpec::new().set_fg(Some(color)).set_bold(true))?;
    write!(stdout, "[{:width$}]", label, width = max_label_len)?;
    stdout.reset()?;
    writeln!(stdout, " {line}")?;

    Ok(())
}

fn monitor_processes(
    mut children: Vec<(String, Child)>,
) -> (sync::oneshot::Sender<()>, JoinHandle<()>) {
    let (shutdown_signal, on_shutdown) = sync::oneshot::channel::<()>();

    let task = tokio::spawn(async move {
        while !on_shutdown.is_terminated() {
            let mut any_exited = false;
            for (label, child) in &mut children {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        eprintln!("\nProcess {} exited with status: {}", label, status);
                        any_exited = true;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("\nError checking process {}: {}", label, e);
                        any_exited = true;
                    }
                }
            }
            if any_exited {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        cleanup_processes(&mut children).await;
    });

    (shutdown_signal, task)
}

async fn cleanup_processes(children: &mut Vec<(String, Child)>) {
    eprintln!("\nCleaning up processes...\n");

    for (label, child) in children.iter_mut() {
        if let Err(e) = child.start_kill() {
            eprintln!("Failed to kill process {}: {}", label, e);
        }
    }

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    for (label, child) in children.iter_mut() {
        match child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                eprintln!("Force killing process {}", label);
                let _ = child.kill().await;
            }
            Err(_) => {
                let _ = child.kill().await;
            }
        }
    }
}
