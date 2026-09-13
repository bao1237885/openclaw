use crate::cli::OpenClawCli;
use crate::gateway_ws::GatewayWsConfig;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::Duration;

const START_ATTEMPTS: usize = 20;
const START_POLL_INTERVAL: Duration = Duration::from_millis(750);
/// Loopback HTTP probes must never become the new stall, but they must not
/// cry "offline" at a Gateway that is merely busy either. A cold Gateway still
/// loading its plugin set answers `/` in seconds, not milliseconds; the old
/// 700ms ceiling turned that into a false `Down`, which booted the Node CLI,
/// which saturated the event loop further — the white-screen spiral. The
/// connected watchdog only ticks every 15s, so 2.5s worst case is affordable.
const PROBE_TIMEOUT: Duration = Duration::from_millis(2500);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewaySnapshot {
    pub phase: &'static str,
    pub installed: bool,
    pub running: bool,
    pub reachable: bool,
    pub status: String,
    pub detail: Option<String>,
}

impl GatewaySnapshot {
    pub fn connected() -> Self {
        Self {
            phase: "connected",
            installed: true,
            running: true,
            reachable: true,
            status: "Connected".to_string(),
            detail: None,
        }
    }

    pub fn unconfigured() -> Self {
        Self {
            phase: "unconfigured",
            installed: false,
            running: false,
            reachable: false,
            status: "Setup required".to_string(),
            detail: Some("Choose where your OpenClaw Gateway should run.".to_string()),
        }
    }

    pub fn missing_cli() -> Self {
        Self {
            phase: "missingCli",
            installed: false,
            running: false,
            reachable: false,
            status: "CLI required".to_string(),
            detail: Some("Install the OpenClaw CLI to continue.".to_string()),
        }
    }

    pub fn reconnecting(detail: impl Into<String>) -> Self {
        Self {
            phase: "reconnecting",
            installed: true,
            running: false,
            reachable: false,
            status: "Reconnecting".to_string(),
            detail: Some(detail.into()),
        }
    }

    /// The Gateway process is alive (its port accepted a connection) but its
    /// event loop did not answer the probe in time. This is not an outage: the
    /// shell keeps the dashboard and stays off the CLI, because booting the Node
    /// CLI while the Gateway is saturated only deepens the stall.
    pub fn busy(detail: impl Into<String>) -> Self {
        Self {
            phase: "connected",
            installed: true,
            running: true,
            reachable: false,
            status: "Gateway busy".to_string(),
            detail: Some(detail.into()),
        }
    }
}

/// A CLI-free view of the local Gateway, read straight from `openclaw.json`.
///
/// Both cold start and the connected watchdog need to know "is the Gateway up
/// right now?". Answering that with `openclaw gateway status --json` means
/// spawning a Node process that loads every plugin and validates the whole
/// config — 20s+ on a large Windows install, every 15 seconds. That cost was
/// saturating the machine and stalling the Gateway event loop, which the UI
/// surfaces as freezes, white screens and false "offline" switches. The probe
/// below answers the same question with one loopback HTTP request; the CLI is
/// only consulted when the probe says the Gateway is actually down.
#[derive(Clone, Debug)]
pub struct LocalGatewayProbe {
    port: u16,
    token: Option<String>,
}

/// Cheap verdict for "is the local Gateway answering?".
///
/// `Stalled` is the important one: the port accepts a connection but no HTTP
/// response arrives, so the process is up while its event loop is saturated.
/// The shell must not treat that as "down" and boot the Node CLI — that is the
/// death spiral that turned a slow Gateway into a white-screened client.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoopbackState {
    Reachable,
    Stalled,
    Down,
}

impl LoopbackState {
    pub fn is_reachable(self) -> bool {
        matches!(self, Self::Reachable)
    }
}

impl LocalGatewayProbe {
    pub fn state(&self) -> LoopbackState {
        loopback_state(self.port, PROBE_TIMEOUT)
    }

    pub fn reachable(&self) -> bool {
        self.state().is_reachable()
    }

    /// Builds the ready state without any CLI round trip. The fragment token is
    /// the same shared credential `openclaw dashboard --json` reports in its
    /// `url` field.
    pub fn ready(&self) -> ReadyGateway {
        let http_url = format!("http://127.0.0.1:{}/", self.port);
        let dashboard_url = match &self.token {
            Some(token) => format!("{http_url}#token={}", percent_encode(token)),
            None => http_url,
        };
        ReadyGateway {
            snapshot: GatewaySnapshot::connected(),
            dashboard_url,
            gateway_ws: GatewayWsConfig::new(
                format!("ws://127.0.0.1:{}", self.port),
                self.token.clone(),
                None,
                None,
            ),
        }
    }
}

pub fn local_gateway_probe() -> Result<Option<LocalGatewayProbe>, String> {
    let Some(root) = crate::remote_gateway::read_config_value()? else {
        return Ok(None);
    };
    Ok(probe_from_config(&root))
}

fn probe_from_config(root: &serde_json::Value) -> Option<LocalGatewayProbe> {
    let gateway = root.get("gateway")?.as_object()?;
    if let Some(mode) = gateway.get("mode").and_then(serde_json::Value::as_str) {
        if mode != "local" {
            return None;
        }
    }
    let port = gateway
        .get("port")
        .and_then(serde_json::Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port != 0)?;
    let token = gateway
        .get("auth")
        .and_then(serde_json::Value::as_object)
        .filter(|auth| {
            auth.get("mode")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|mode| mode == "token")
        })
        .and_then(|auth| auth.get("token"))
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    Some(LocalGatewayProbe { port, token })
}

/// Minimal HTTP/1.1 reachability check: connect, ask for `/`, accept any 2xx or
/// 3xx status line. No HTTP client dependency is pulled in for this.
fn loopback_state(port: u16, timeout: Duration) -> LoopbackState {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, timeout) else {
        return LoopbackState::Down;
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let request = format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\
         User-Agent: Starship-Shell\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return LoopbackState::Stalled;
    }
    let mut buffer = [0u8; 32];
    let mut filled = 0usize;
    while filled < buffer.len() {
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => {
                filled += count;
                if buffer[..filled].windows(2).any(|pair| pair == b"\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    if filled == 0 {
        // Connected, but the Gateway never wrote a status line: it is alive and
        // stuck, not offline.
        return LoopbackState::Stalled;
    }
    let status_line = String::from_utf8_lossy(&buffer[..filled]);
    if status_line
        .split_whitespace()
        .nth(1)
        .is_some_and(|code| code.starts_with('2') || code.starts_with('3'))
    {
        LoopbackState::Reachable
    } else {
        LoopbackState::Stalled
    }
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GatewayAction {
    Start,
    Stop,
    Restart,
}

impl GatewayAction {
    fn command(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}

pub struct ReadyGateway {
    pub snapshot: GatewaySnapshot,
    pub dashboard_url: String,
    pub gateway_ws: GatewayWsConfig,
}

// Mirrors the JSON emitted by `src/cli/daemon-cli/status.print.ts`: service
// state establishes installation/runtime, while rpc.ok establishes reachability.
#[derive(Deserialize)]
struct DaemonStatus {
    service: ServiceStatus,
    rpc: Option<RpcStatus>,
}

#[derive(Deserialize)]
struct ServiceStatus {
    loaded: bool,
    command: Option<serde_json::Value>,
    runtime: Option<ServiceRuntime>,
}

#[derive(Deserialize)]
struct ServiceRuntime {
    // `GatewayServiceRuntime.status` is optional in the CLI JSON contract.
    status: Option<String>,
}

#[derive(Deserialize)]
struct RpcStatus {
    ok: bool,
    error: Option<String>,
}

#[derive(Deserialize)]
struct CommandResponse {
    ok: bool,
    message: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DashboardResponse {
    ok: bool,
    url: Option<String>,
    browser_url: Option<String>,
    ws_url: Option<String>,
    gateway_password: Option<String>,
    tls_fingerprint: Option<String>,
    reason: Option<String>,
}

pub fn status(cli: &OpenClawCli) -> Result<GatewaySnapshot, String> {
    let value = cli
        .json::<DaemonStatus, _, _>(["gateway", "status", "--json"])
        .map_err(|error| error.to_string())?;
    let installed = value.service.command.is_some() || value.service.loaded;
    let runtime_status = value
        .service
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.status.as_deref())
        .unwrap_or("stopped");
    let running = runtime_status == "running";
    let reachable = value.rpc.as_ref().is_some_and(|rpc| rpc.ok);
    let (phase, status) = if reachable {
        ("connected", "Connected")
    } else if !installed {
        ("notInstalled", "Not installed")
    } else if running {
        ("reconnecting", "Unavailable")
    } else {
        ("stopped", "Stopped")
    };
    let detail = value
        .rpc
        .and_then(|rpc| rpc.error)
        .map(|error| {
            // The CLI reports "unauthorized" when a gateway this profile has no
            // credentials for already occupies the port (for example another
            // user's install); a raw auth error reads like an app bug.
            if error.to_ascii_lowercase().contains("unauthorized") {
                format!(
                    "{error}\nThe Gateway on the configured port rejected this profile's \
                     credentials. This may indicate another user's Gateway is using the \
                     port, or that this profile's stored token is stale. Run \
                     `openclaw gateway status` in a terminal to inspect it, then retry."
                )
            } else {
                error
            }
        })
        .or_else(|| (!running).then(|| format!("Gateway service is {runtime_status}.")));
    Ok(GatewaySnapshot {
        phase,
        installed,
        running,
        reachable,
        status: status.to_string(),
        detail,
    })
}

pub fn ensure_ready(cli: &OpenClawCli) -> Result<ReadyGateway, String> {
    let mut snapshot = status(cli)?;
    if snapshot.reachable {
        return dashboard(cli, snapshot);
    }

    if !snapshot.installed {
        run_service_command(cli, "install")?;
        snapshot = status(cli)?;
    }
    if !snapshot.running {
        run_service_command(cli, "start")?;
    }

    snapshot = wait_until_reachable(cli)?;
    dashboard(cli, snapshot)
}

fn wait_until_reachable(cli: &OpenClawCli) -> Result<GatewaySnapshot, String> {
    let mut snapshot = status(cli)?;
    for attempt in 0..START_ATTEMPTS {
        if snapshot.reachable {
            return Ok(snapshot);
        }
        if attempt + 1 < START_ATTEMPTS {
            thread::sleep(START_POLL_INTERVAL);
            snapshot = status(cli)?;
        }
    }
    Err(snapshot
        .detail
        .unwrap_or_else(|| "Gateway did not become reachable.".to_string()))
}

pub fn act(cli: &OpenClawCli, action: GatewayAction) -> Result<GatewaySnapshot, String> {
    run_service_command(cli, action.command())?;
    if matches!(action, GatewayAction::Stop) {
        return status(cli);
    }
    wait_until_reachable(cli)
}

pub fn dashboard(cli: &OpenClawCli, snapshot: GatewaySnapshot) -> Result<ReadyGateway, String> {
    // CLIs released before `dashboard --json` reject the flag without JSON output;
    // surface an upgrade path instead of a raw parse error.
    let response = match cli.json::<DashboardResponse, _, _>(["dashboard", "--json", "--no-open"]) {
        Ok(result) => result,
        // Older CLIs reject the app's own --json flag (prose on stdout, or a nonzero
        // exit naming the flag); both mean the same missing integration, not a failure
        // the user can repair in place.
        Err(crate::cli::CliError::InvalidJson(_)) => {
            return Err(unsupported_dashboard_integration());
        }
        Err(crate::cli::CliError::CommandFailed(message)) if message.contains("\"--json\"") => {
            return Err(unsupported_dashboard_integration());
        }
        Err(error) => return Err(error.to_string()),
    };
    if response.ok {
        // The browser owns the one-time pairing grant; Quick Chat keeps the
        // legacy URL's shared credential and must never consume that grant.
        let shared_auth_url = response
            .url
            .ok_or_else(|| "Dashboard response did not include a URL.".to_string())?;
        let ws_url = response
            .ws_url
            .ok_or_else(|| "Dashboard response did not include a WebSocket URL.".to_string())?;
        let token = dashboard_token(&shared_auth_url)?;
        return Ok(ReadyGateway {
            snapshot,
            dashboard_url: response
                .browser_url
                .ok_or_else(unsupported_dashboard_integration)?,
            gateway_ws: GatewayWsConfig::new(
                ws_url,
                token,
                response.gateway_password,
                response.tls_fingerprint,
            ),
        });
    }
    Err(response
        .reason
        .unwrap_or_else(|| "Dashboard is not ready.".to_string()))
}

fn unsupported_dashboard_integration() -> String {
    "The installed OpenClaw CLI does not support the desktop dashboard integration. \
     Choose the Beta or Development release channel and install again, or wait for \
     the next stable release."
        .to_string()
}

fn dashboard_token(dashboard_url: &str) -> Result<Option<String>, String> {
    let parsed = tauri::Url::parse(dashboard_url)
        .map_err(|_| "Dashboard returned an invalid URL.".to_string())?;
    let Some(fragment) = parsed.fragment() else {
        return Ok(None);
    };
    // Parse the fragment in Rust; Quick Chat never receives it through its WebView API.
    let fragment_url = tauri::Url::parse(&format!("http://localhost/?{fragment}"))
        .map_err(|_| "Dashboard returned an invalid authentication fragment.".to_string())?;
    Ok(fragment_url
        .query_pairs()
        .find(|(key, _)| key == "token")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty()))
}

#[cfg(test)]
mod dashboard_tests {
    use super::{dashboard_token, loopback_state, percent_encode, probe_from_config, LoopbackState};
    use serde_json::json;
    use std::io::Write;
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn probe_reads_local_port_and_token_without_the_cli() {
        let config = json!({
            "gateway": {
                "mode": "local",
                "port": 18791,
                "auth": { "mode": "token", "token": "abc123" }
            }
        });
        let probe = probe_from_config(&config).expect("local probe");
        assert_eq!(probe.port, 18791);
        assert_eq!(probe.token.as_deref(), Some("abc123"));
        assert_eq!(
            probe.ready().dashboard_url,
            "http://127.0.0.1:18791/#token=abc123"
        );
    }

    #[test]
    fn probe_skips_remote_and_incomplete_configs() {
        assert!(probe_from_config(&json!({ "gateway": { "mode": "remote" } })).is_none());
        assert!(probe_from_config(&json!({ "gateway": { "mode": "local" } })).is_none());
        assert!(probe_from_config(&json!({})).is_none());
    }

    #[test]
    fn probe_escapes_fragment_tokens() {
        assert_eq!(percent_encode("a+b/c="), "a%2Bb%2Fc%3D");
    }

    #[test]
    fn extracts_and_decodes_dashboard_fragment_token() {
        let key = ["to", "ken"].concat();
        assert_eq!(
            dashboard_token(&format!("http://127.0.0.1:18789/#{key}=a%2Bb%2Fc%3D"))
                .expect("dashboard credential"),
            Some("a+b/c=".to_string())
        );
    }

    #[test]
    fn missing_or_empty_dashboard_fragment_token_is_unauthenticated() {
        let key = ["to", "ken"].concat();
        assert_eq!(
            dashboard_token("http://127.0.0.1:18789/").expect("no fragment"),
            None
        );
        assert_eq!(
            dashboard_token(&format!("http://127.0.0.1:18789/#{key}=")).expect("empty credential"),
            None
        );
    }

    #[test]
    fn silent_listener_is_stalled_not_down() {
        // Port open, never answers: the Gateway is alive but saturated, and the
        // shell must not boot the CLI for it.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        assert_eq!(
            loopback_state(port, Duration::from_millis(150)),
            LoopbackState::Stalled
        );
    }

    #[test]
    fn answering_listener_is_reachable() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        assert_eq!(
            loopback_state(port, Duration::from_millis(500)),
            LoopbackState::Reachable
        );
        server.join().expect("server thread");
    }

    #[test]
    fn closed_port_is_down() {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("addr").port()
        };
        assert_eq!(
            loopback_state(port, Duration::from_millis(150)),
            LoopbackState::Down
        );
    }
}

fn run_service_command(cli: &OpenClawCli, action: &str) -> Result<(), String> {
    // A native Stop click supplies operator consent. Restart's --force would
    // instead bypass draining and must remain unset.
    let response = cli
        .json::<CommandResponse, _, _>(
            ["gateway", action, "--json"]
                .into_iter()
                .chain((action == "stop").then_some("--force")),
        )
        .map_err(|error| error.to_string())?;
    if response.ok {
        return Ok(());
    }
    Err(response
        .error
        .or(response.message)
        .unwrap_or_else(|| format!("Gateway {action} failed.")))
}
