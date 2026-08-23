//! Debug watcher for the Firedancer GUI websocket.

use std::{
    fs,
    io::{self, ErrorKind, IsTerminal, Write},
    path::Path,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use tracing::info;
use tungstenite::{Message, connect, stream::MaybeTlsStream};

const DEFAULT_GUI_ADDRESS: &str = "127.0.0.1";
const DEFAULT_GUI_PORT: u16 = 80;
const RETRY_INTERVAL: Duration = Duration::from_secs(1);
const STATE_COLUMN: usize = 42;

/// Connects to the Firedancer GUI websocket and prints boot/startup state.
pub fn run(config_path: &Path, url_override: Option<&str>, all: bool) -> Result<()> {
    let url = match url_override {
        Some(url) => url.to_owned(),
        None => websocket_url_from_config(config_path)?,
    };
    info!(url = %url, all, "connecting to Firedancer GUI websocket");
    let stdout = io::stdout();
    let live = stdout.is_terminal() && !all;
    watch(&url, all, live, &mut stdout.lock())
}

/// Reads the GUI listen address from a Firedancer TOML config.
fn websocket_url_from_config(config_path: &Path) -> Result<String> {
    let config_text = fs::read_to_string(config_path).with_context(|| {
        format!(
            "could not read active Firedancer config {}",
            config_path.display()
        )
    })?;
    let endpoint = GuiEndpoint::from_toml(&config_text).with_context(|| {
        format!(
            "could not parse Firedancer GUI settings from {}",
            config_path.display()
        )
    })?;
    if !endpoint.enabled {
        bail!(
            "Firedancer GUI is disabled in {} ([tiles.gui].enabled = false)",
            config_path.display()
        );
    }
    Ok(endpoint.websocket_url())
}

#[derive(Debug, PartialEq, Eq)]
struct GuiEndpoint {
    enabled: bool,
    address: String,
    port: u16,
}

impl GuiEndpoint {
    fn from_toml(config_text: &str) -> Result<Self> {
        let config: FiredancerConfig = toml::from_str(config_text)?;
        let gui = config.tiles.gui;
        Ok(Self {
            enabled: gui.enabled.unwrap_or(true),
            address: connect_address(
                &gui.gui_listen_address
                    .unwrap_or_else(|| DEFAULT_GUI_ADDRESS.to_owned()),
            ),
            port: gui.gui_listen_port.unwrap_or(DEFAULT_GUI_PORT),
        })
    }

    fn websocket_url(&self) -> String {
        websocket_url(&self.address, self.port)
    }
}

#[derive(Debug, Deserialize, Default)]
struct FiredancerConfig {
    #[serde(default)]
    tiles: TilesConfig,
}

#[derive(Debug, Deserialize, Default)]
struct TilesConfig {
    #[serde(default)]
    gui: GuiConfig,
}

#[derive(Debug, Deserialize, Default)]
struct GuiConfig {
    enabled: Option<bool>,
    gui_listen_address: Option<String>,
    gui_listen_port: Option<u16>,
}

/// Rewrites wildcard bind addresses to a loopback address a client can connect to.
fn connect_address(listen_address: &str) -> String {
    match listen_address {
        "" | "0.0.0.0" => DEFAULT_GUI_ADDRESS.to_owned(),
        "::" | "[::]" => "::1".to_owned(),
        address => address.to_owned(),
    }
}

fn websocket_url(address: &str, port: u16) -> String {
    let host = if address.contains(':') && !address.starts_with('[') {
        format!("[{address}]")
    } else {
        address.to_owned()
    };
    format!("ws://{host}:{port}/websocket")
}

fn watch(url: &str, all: bool, live: bool, out: &mut impl Write) -> Result<()> {
    watch_with_retry(url, all, live, out, RETRY_INTERVAL, None)
}

fn watch_with_retry(
    url: &str,
    all: bool,
    live: bool,
    out: &mut impl Write,
    retry_interval: Duration,
    max_sessions: Option<usize>,
) -> Result<()> {
    let started = Instant::now();
    if all {
        writeln!(out, "dumping every websocket message; Ctrl+C to stop")?;
        out.flush().context("could not write monitor output")?;
    }

    let mut unavailable = false;
    let mut state = CurrentState::new(live);
    let mut sessions = 0;
    loop {
        match watch_session(url, all, started, &mut unavailable, &mut state, out) {
            Ok(()) => {
                sessions += 1;
                if max_sessions.is_some_and(|max| sessions >= max) {
                    return Ok(());
                }
                announce_unavailable(all, started, &mut unavailable, &mut state, out)?;
            }
            Err(error) if is_unavailable(&error) => {
                announce_unavailable(all, started, &mut unavailable, &mut state, out)?;
            }
            Err(error) => return Err(error),
        }
        thread::sleep(retry_interval);
        state.tick(out)?;
    }
}

fn announce_unavailable(
    all: bool,
    started: Instant,
    unavailable: &mut bool,
    state: &mut CurrentState,
    out: &mut impl Write,
) -> Result<()> {
    if all && !*unavailable {
        writeln!(
            out,
            "+{:.1}s  service not available; retrying",
            elapsed_secs(started)
        )?;
        out.flush().context("could not write monitor output")?;
        *unavailable = true;
    } else if !all {
        state.transition("service not available; retrying", out)?;
    }
    Ok(())
}

fn watch_session(
    url: &str,
    all: bool,
    started: Instant,
    unavailable: &mut bool,
    state: &mut CurrentState,
    out: &mut impl Write,
) -> Result<()> {
    let (mut socket, _response) =
        connect(url).with_context(|| format!("could not connect to Firedancer GUI at {url}"))?;
    *unavailable = false;
    if all {
        writeln!(out, "+{:.1}s  connected  {url}", elapsed_secs(started))?;
        out.flush().context("could not write monitor output")?;
    } else {
        state.transition("connected; waiting for validator state", out)?;
    }
    if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
        stream
            .set_read_timeout(Some(RETRY_INTERVAL))
            .context("could not configure monitor refresh interval")?;
    }

    loop {
        match socket.read() {
            Ok(Message::Text(payload)) => {
                handle_text(payload.as_str(), all, started, state, out)?;
            }
            Ok(Message::Binary(_)) => {
                if all {
                    writeln!(
                        out,
                        "+{:.1}s  <binary websocket frame ignored>",
                        elapsed_secs(started)
                    )?;
                    out.flush().context("could not write monitor output")?;
                }
            }
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {
                state.tick(out)?;
            }
            Ok(Message::Close(_)) => return Ok(()),
            Err(tungstenite::Error::Io(error)) if is_refresh_timeout(&error) => {
                state.tick(out)?;
            }
            Err(error) if is_unavailable_websocket(&error) => return Ok(()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Firedancer GUI websocket {url} closed"));
            }
        }
    }
}

fn handle_text(
    payload: &str,
    all: bool,
    started: Instant,
    state: &mut CurrentState,
    out: &mut impl Write,
) -> Result<()> {
    let elapsed = elapsed_secs(started);
    let Ok(message) = serde_json::from_str::<GuiMessage>(payload) else {
        if all {
            writeln!(out, "+{elapsed:.1}s  <unparsed>\n{payload}")?;
            out.flush().context("could not write monitor output")?;
        } else {
            state.tick(out)?;
        }
        return Ok(());
    };

    let name = format!("{}.{}", message.topic, message.key);
    if all {
        writeln!(out, "+{elapsed:.1}s  {name}")?;
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&message.value)
                .unwrap_or_else(|_| message.value.to_string())
        )?;
        out.flush().context("could not write monitor output")?;
    } else if is_progress_key(&message.topic, &message.key)
        && let Some(phase) = message.value.get("phase").and_then(Value::as_str)
    {
        state.transition(&humanize_phase(phase), out)?;
    } else {
        state.tick(out)?;
    }
    Ok(())
}

#[derive(Debug)]
struct CurrentState {
    active: Option<ActiveState>,
    live: bool,
}

#[derive(Debug)]
struct ActiveState {
    label: String,
    started: Instant,
    rendered_second: u64,
}

impl CurrentState {
    fn new(live: bool) -> Self {
        Self { active: None, live }
    }

    fn transition(&mut self, label: &str, out: &mut impl Write) -> Result<()> {
        self.transition_at(label, Instant::now(), out)
    }

    fn transition_at(&mut self, label: &str, now: Instant, out: &mut impl Write) -> Result<()> {
        if self
            .active
            .as_ref()
            .is_some_and(|state| state.label == label)
        {
            return self.tick_at(now, out);
        }

        if self.live && self.active.is_some() {
            self.render_at(now, out)?;
            writeln!(out)?;
        }

        self.active = Some(ActiveState {
            label: label.to_owned(),
            started: now,
            rendered_second: 0,
        });
        if self.live {
            self.render_at(now, out)?;
        } else {
            writeln!(out, "{}", state_line(label, Duration::ZERO))?;
            out.flush().context("could not write monitor output")?;
        }
        Ok(())
    }

    fn tick(&mut self, out: &mut impl Write) -> Result<()> {
        self.tick_at(Instant::now(), out)
    }

    fn tick_at(&mut self, now: Instant, out: &mut impl Write) -> Result<()> {
        let Some(active) = self.active.as_ref() else {
            return Ok(());
        };
        if !self.live || now.duration_since(active.started).as_secs() == active.rendered_second {
            return Ok(());
        }
        self.render_at(now, out)
    }

    fn render_at(&mut self, now: Instant, out: &mut impl Write) -> Result<()> {
        let Some(active) = self.active.as_mut() else {
            return Ok(());
        };
        let elapsed = now.duration_since(active.started);
        active.rendered_second = elapsed.as_secs();
        write!(out, "\r{}\x1b[K", state_line(&active.label, elapsed))?;
        out.flush().context("could not write monitor output")
    }
}

fn humanize_phase(phase: &str) -> String {
    phase.replace('_', " ")
}

fn state_line(label: &str, elapsed: Duration) -> String {
    let dots = STATE_COLUMN
        .saturating_sub(label.len().saturating_add(1))
        .max(3);
    format!(
        "{label} {} {}",
        ".".repeat(dots),
        crate::progress::format_duration(elapsed)
    )
}

#[derive(Debug, Deserialize)]
struct GuiMessage {
    topic: String,
    key: String,
    #[serde(default)]
    value: Value,
}

fn is_progress_key(topic: &str, key: &str) -> bool {
    topic == "summary" && matches!(key, "startup_progress" | "boot_progress")
}

fn elapsed_secs(started: Instant) -> f64 {
    started.elapsed().as_secs_f64()
}

fn is_unavailable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<tungstenite::Error>()
            .is_some_and(is_unavailable_websocket)
            || cause
                .downcast_ref::<io::Error>()
                .is_some_and(is_unavailable_io)
    })
}

fn is_unavailable_websocket(error: &tungstenite::Error) -> bool {
    match error {
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => true,
        tungstenite::Error::Io(io_error) => is_unavailable_io(io_error),
        tungstenite::Error::Http(_)
        | tungstenite::Error::HttpFormat(_)
        | tungstenite::Error::Protocol(_) => true,
        tungstenite::Error::Url(tungstenite::error::UrlError::UnableToConnect(_)) => true,
        _ => false,
    }
}

fn is_unavailable_io(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::UnexpectedEof
            | ErrorKind::TimedOut
            | ErrorKind::NotConnected
            | ErrorKind::AddrNotAvailable
            | ErrorKind::WouldBlock
    )
}

fn is_refresh_timeout(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        net::TcpListener,
        thread,
        time::{Duration, Instant},
    };

    use anyhow::Result;
    use tungstenite::{Message, accept};

    use super::{
        CurrentState, GuiEndpoint, connect_address, is_progress_key, is_unavailable, watch_session,
        watch_with_retry, websocket_url,
    };

    fn serve_one_session(server: TcpListener, payload: &'static str) {
        thread::spawn(move || {
            let (stream, _) = server.accept().expect("accept");
            let mut socket = accept(stream).expect("websocket handshake");
            socket
                .send(Message::Text(payload.into()))
                .expect("send payload");
            socket.send(Message::Close(None)).expect("close");
            thread::sleep(Duration::from_millis(50));
        });
    }

    #[test]
    fn uses_firedancer_gui_defaults() -> Result<()> {
        let endpoint = GuiEndpoint::from_toml("")?;
        assert_eq!(
            endpoint,
            GuiEndpoint {
                enabled: true,
                address: "127.0.0.1".to_owned(),
                port: 80,
            }
        );
        assert_eq!(endpoint.websocket_url(), "ws://127.0.0.1:80/websocket");
        Ok(())
    }

    #[test]
    fn reads_configured_gui_listen_settings() -> Result<()> {
        let endpoint = GuiEndpoint::from_toml(
            "[tiles.gui]\nenabled = true\ngui_listen_address = \"10.0.0.8\"\ngui_listen_port = 8080\n",
        )?;
        assert_eq!(endpoint.address, "10.0.0.8");
        assert_eq!(endpoint.port, 8080);
        assert_eq!(endpoint.websocket_url(), "ws://10.0.0.8:8080/websocket");
        Ok(())
    }

    #[test]
    fn rejects_disabled_gui() -> Result<()> {
        let endpoint = GuiEndpoint::from_toml("[tiles.gui]\nenabled = false\n")?;
        assert!(!endpoint.enabled);
        Ok(())
    }

    #[test]
    fn rewrites_wildcard_bind_addresses() {
        assert_eq!(connect_address("0.0.0.0"), "127.0.0.1");
        assert_eq!(connect_address("::"), "::1");
        assert_eq!(websocket_url("::1", 80), "ws://[::1]:80/websocket");
    }

    #[test]
    fn run_fails_when_gui_is_disabled() {
        let temp = tempfile::TempDir::new().expect("temporary directory");
        let config = temp.path().join("active-fd-config.toml");
        std::fs::write(&config, "[tiles.gui]\nenabled = false\n").expect("write config");
        let error = super::run(&config, None, false).expect_err("disabled GUI");
        assert!(format!("{error:#}").contains("disabled"), "{error:#}");
    }

    #[test]
    fn progress_filter_keeps_boot_keys() {
        assert!(is_progress_key("summary", "startup_progress"));
        assert!(is_progress_key("summary", "boot_progress"));
        assert!(!is_progress_key("summary", "estimated_tps"));
    }

    #[test]
    fn connection_refused_is_unavailable() {
        let error = anyhow::Error::from(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "connection refused",
        ));
        assert!(is_unavailable(&error));
    }

    #[test]
    fn tungstenite_unable_to_connect_is_unavailable() {
        let error = anyhow::Error::from(tungstenite::Error::Url(
            tungstenite::error::UrlError::UnableToConnect("ws://127.0.0.1:80/websocket".to_owned()),
        ))
        .context("could not connect to Firedancer GUI at ws://127.0.0.1:80/websocket");
        assert!(is_unavailable(&error), "{error:#}");
    }

    #[test]
    fn prints_only_validator_state_transitions() -> Result<()> {
        let server = TcpListener::bind("127.0.0.1:0")?;
        let addr = server.local_addr()?;
        let url = format!("ws://{addr}/websocket");
        let thread = thread::spawn(move || {
            let (stream, _) = server.accept().expect("accept");
            let mut socket = accept(stream).expect("websocket handshake");
            socket
                .send(Message::Text(
                    r#"{"topic":"summary","key":"cluster","value":"mainnet-beta"}"#.into(),
                ))
                .expect("send cluster");
            socket
                .send(Message::Text(
                    r#"{"topic":"summary","key":"startup_progress","value":{"phase":"downloading_full_snapshot","downloading_full_snapshot_current_bytes":100}}"#.into(),
                ))
                .expect("send progress");
            socket
                .send(Message::Text(
                    r#"{"topic":"summary","key":"cluster","value":"mainnet-beta"}"#.into(),
                ))
                .expect("send duplicate cluster");
            socket.send(Message::Close(None)).expect("close");
            thread::sleep(Duration::from_millis(50));
        });

        let mut output = Vec::new();
        let mut unavailable = false;
        let mut state = CurrentState::new(false);
        watch_session(
            &url,
            false,
            Instant::now(),
            &mut unavailable,
            &mut state,
            &mut output,
        )?;
        thread.join().expect("server thread");

        let text = String::from_utf8(output)?;
        assert!(
            text.contains("connected; waiting for validator state"),
            "{text}"
        );
        assert!(text.contains("downloading full snapshot"), "{text}");
        assert!(!text.contains("summary.cluster"), "{text}");
        assert!(!text.contains("current_bytes"), "{text}");
        Ok(())
    }

    #[test]
    fn all_mode_prints_non_progress_payloads() -> Result<()> {
        let server = TcpListener::bind("127.0.0.1:0")?;
        let addr = server.local_addr()?;
        let url = format!("ws://{addr}/websocket");
        serve_one_session(
            server,
            r#"{"topic":"summary","key":"cluster","value":"testnet"}"#,
        );

        let mut output = Vec::new();
        let mut unavailable = false;
        let mut state = CurrentState::new(false);
        watch_session(
            &url,
            true,
            Instant::now(),
            &mut unavailable,
            &mut state,
            &mut output,
        )?;

        let text = String::from_utf8(output)?;
        assert!(text.contains("summary.cluster"), "{text}");
        assert!(text.contains("testnet"), "{text}");
        assert!(!text.contains("[hidden;"), "{text}");
        Ok(())
    }

    #[test]
    fn reconnects_after_the_service_drops() -> Result<()> {
        let server = TcpListener::bind("127.0.0.1:0")?;
        let addr = server.local_addr()?;
        let url = format!("ws://{addr}/websocket");
        let thread = thread::spawn(move || {
            for payload in [
                r#"{"topic":"summary","key":"startup_progress","value":{"phase":"downloading_full_snapshot"}}"#,
                r#"{"topic":"summary","key":"startup_progress","value":{"phase":"running"}}"#,
            ] {
                let (stream, _) = server.accept().expect("accept");
                let mut socket = accept(stream).expect("websocket handshake");
                socket
                    .send(Message::Text(payload.into()))
                    .expect("send payload");
                socket.send(Message::Close(None)).expect("close");
            }
        });

        let mut output = Vec::new();
        watch_with_retry(
            &url,
            false,
            false,
            &mut output,
            Duration::from_millis(20),
            Some(2),
        )?;
        thread.join().expect("server thread");

        let text = String::from_utf8(output)?;
        assert_eq!(text.matches("connected").count(), 2, "{text}");
        assert!(text.contains("service not available; retrying"), "{text}");
        assert!(text.contains("downloading full snapshot"), "{text}");
        assert!(text.contains("running"), "{text}");
        Ok(())
    }

    #[test]
    fn live_state_counts_up_and_starts_a_new_line_on_transition() -> Result<()> {
        let started = Instant::now();
        let mut state = CurrentState::new(true);
        let mut output = Vec::new();

        state.transition_at("loading ledger", started, &mut output)?;
        state.tick_at(started + Duration::from_secs(3), &mut output)?;
        state.transition_at(
            "processing ledger",
            started + Duration::from_secs(5),
            &mut output,
        )?;

        let text = String::from_utf8(output)?;
        assert!(text.contains("loading ledger"), "{text}");
        assert!(text.contains("3s"), "{text}");
        assert!(text.contains("5s"), "{text}");
        assert!(text.contains("processing ledger"), "{text}");
        assert_eq!(text.matches('\n').count(), 1, "{text:?}");
        Ok(())
    }
}
