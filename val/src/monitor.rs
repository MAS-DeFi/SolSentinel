//! Debug watcher for the Firedancer GUI websocket.

use std::{
    collections::BTreeSet,
    fs,
    io::{self, Write},
    path::Path,
    time::Instant,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use tracing::info;
use tungstenite::{Message, connect};

const DEFAULT_GUI_ADDRESS: &str = "127.0.0.1";
const DEFAULT_GUI_PORT: u16 = 80;

/// Connects to the Firedancer GUI websocket and prints boot/startup state.
pub fn run(config_path: &Path, url_override: Option<&str>, all: bool) -> Result<()> {
    let url = match url_override {
        Some(url) => url.to_owned(),
        None => websocket_url_from_config(config_path)?,
    };
    info!(url = %url, all, "connecting to Firedancer GUI websocket");
    watch(&url, all, &mut io::stdout().lock())
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

fn watch(url: &str, all: bool, out: &mut impl Write) -> Result<()> {
    let (mut socket, _response) = connect(url).with_context(|| {
        format!(
            "could not connect to Firedancer GUI at {url}. Is the validator running with [tiles.gui] enabled?"
        )
    })?;
    let started = Instant::now();
    writeln!(out, "+{:.1}s  connected  {url}", elapsed_secs(started))?;
    if all {
        writeln!(out, "dumping every websocket message; Ctrl+C to stop")?;
    } else {
        writeln!(
            out,
            "showing startup/boot progress; other keys listed once. Pass --all to dump every message. Ctrl+C to stop"
        )?;
    }
    out.flush().context("could not write monitor output")?;

    let mut hidden_keys = BTreeSet::new();
    loop {
        match socket.read() {
            Ok(Message::Text(payload)) => {
                handle_text(payload.as_str(), all, started, &mut hidden_keys, out)?;
            }
            Ok(Message::Binary(_)) => {
                writeln!(
                    out,
                    "+{:.1}s  <binary websocket frame ignored>",
                    elapsed_secs(started)
                )?;
                out.flush().context("could not write monitor output")?;
            }
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
            Ok(Message::Close(_)) => break,
            Err(error) if is_clean_disconnect(&error) => break,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Firedancer GUI websocket {url} closed"));
            }
        }
    }
    writeln!(out, "+{:.1}s  disconnected", elapsed_secs(started))?;
    Ok(())
}

fn handle_text(
    payload: &str,
    all: bool,
    started: Instant,
    hidden_keys: &mut BTreeSet<String>,
    out: &mut impl Write,
) -> Result<()> {
    let elapsed = elapsed_secs(started);
    let Ok(message) = serde_json::from_str::<GuiMessage>(payload) else {
        if all {
            writeln!(out, "+{elapsed:.1}s  <unparsed>\n{payload}")?;
            out.flush().context("could not write monitor output")?;
        }
        return Ok(());
    };

    let name = format!("{}.{}", message.topic, message.key);
    if all || is_progress_key(&message.topic, &message.key) {
        writeln!(out, "+{elapsed:.1}s  {name}")?;
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&message.value)
                .unwrap_or_else(|_| message.value.to_string())
        )?;
    } else if hidden_keys.insert(name.clone()) {
        writeln!(out, "+{elapsed:.1}s  {name}  [hidden; pass --all to print]")?;
    }
    out.flush().context("could not write monitor output")?;
    Ok(())
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

fn is_clean_disconnect(error: &tungstenite::Error) -> bool {
    matches!(
        error,
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed
    )
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, thread, time::Duration};

    use anyhow::Result;
    use tungstenite::{Message, accept};

    use super::{GuiEndpoint, connect_address, is_progress_key, watch, websocket_url};

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
    fn prints_startup_progress_and_hides_other_keys() -> Result<()> {
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
        watch(&url, false, &mut output)?;
        thread.join().expect("server thread");

        let text = String::from_utf8(output)?;
        assert!(text.contains("connected"), "{text}");
        assert!(
            text.contains("summary.cluster  [hidden; pass --all to print]"),
            "{text}"
        );
        assert!(text.contains("summary.startup_progress"), "{text}");
        assert!(text.contains("downloading_full_snapshot"), "{text}");
        assert_eq!(
            text.matches("summary.cluster  [hidden; pass --all to print]")
                .count(),
            1,
            "{text}"
        );
        assert!(text.contains("disconnected"), "{text}");
        Ok(())
    }

    #[test]
    fn all_mode_prints_non_progress_payloads() -> Result<()> {
        let server = TcpListener::bind("127.0.0.1:0")?;
        let addr = server.local_addr()?;
        let url = format!("ws://{addr}/websocket");
        let thread = thread::spawn(move || {
            let (stream, _) = server.accept().expect("accept");
            let mut socket = accept(stream).expect("websocket handshake");
            socket
                .send(Message::Text(
                    r#"{"topic":"summary","key":"cluster","value":"testnet"}"#.into(),
                ))
                .expect("send cluster");
            socket.send(Message::Close(None)).expect("close");
            thread::sleep(Duration::from_millis(50));
        });

        let mut output = Vec::new();
        watch(&url, true, &mut output)?;
        thread.join().expect("server thread");

        let text = String::from_utf8(output)?;
        assert!(text.contains("summary.cluster"), "{text}");
        assert!(text.contains("testnet"), "{text}");
        assert!(!text.contains("[hidden;"), "{text}");
        Ok(())
    }
}
