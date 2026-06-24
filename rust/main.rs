use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use futures::StreamExt;
use libp2p::{
    PeerId, Swarm, SwarmBuilder, core::Multiaddr, identify, identity, multiaddr::Protocol, noise,
    relay, request_response, swarm::StreamProtocol, swarm::SwarmEvent, yamux,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const VERSION: &str = "0.0.1";
const DM_PROTOCOL: &str = "/chatterp2p/dm/0.0.1";
const MAX_MESSAGE_BYTES: usize = 1000;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct DmRequest {
    id: String,
    from: String,
    to: String,
    sent_at: String,
    body: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct DmResponse {
    ok: bool,
}

#[derive(libp2p::swarm::NetworkBehaviour)]
struct Behaviour {
    identify: identify::Behaviour,
    relay: relay::client::Behaviour,
    dm: request_response::json::Behaviour<DmRequest, DmResponse>,
}

#[derive(Debug, Deserialize, Serialize)]
struct IdentityFile {
    #[serde(rename = "type")]
    kind: String,
    private_key_protobuf_base64: String,
    peer_id: String,
    created_at: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct ConfigFile {
    listen: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PeerEntry {
    peer_id: String,
    name: String,
    addresses: Vec<String>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct PeerBook {
    peers: Vec<PeerEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ContactCard {
    peer_id: String,
    #[serde(default)]
    multiaddrs: Vec<String>,
    #[serde(default)]
    multiaddr: Option<String>,
    #[serde(default)]
    direct_addresses: Vec<String>,
    #[serde(default)]
    relay_addresses: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct MessageFile {
    id: String,
    from: String,
    to: String,
    sent_at: String,
    received_at: String,
    body: String,
}

struct DaemonOptions {
    listen: Vec<String>,
    relays: Vec<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(err) = run().await {
        eprintln!(
            "{}",
            json!({ "success": false, "error": err.to_string(), "code": "ERROR" })
        );
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str);

    match cmd {
        None | Some("--help") | Some("-h") => {
            print_usage();
            Ok(())
        }
        Some("--version") | Some("-v") => {
            println!("{VERSION}");
            Ok(())
        }
        Some("--daemon-child") => run_receiver(&args[1..]).await,
        Some("init") => {
            let (peer_id, _) = load_or_create_identity()?;
            ok(json!({
                "peer_id": peer_id.to_string(),
                "paths": {
                    "identity": identity_path(),
                    "config": config_path(),
                    "peers": peers_path(),
                    "messages": messages_path()
                }
            }));
            Ok(())
        }
        Some("me") => {
            let (peer_id, _) = load_identity()?;
            let cfg = load_config()?;
            ok(json!({ "peer_id": peer_id.to_string(), "listen": cfg.listen }));
            Ok(())
        }
        Some("peer") => run_peer(&args[1..]).await,
        Some("message") => {
            let name = args
                .get(1)
                .context("Usage: chatterp2p message <name-or-peer-id> <message>")?;
            let body = args[2..].join(" ");
            if body.is_empty() {
                bail!("Usage: chatterp2p message <name-or-peer-id> <message>");
            }
            let peer = find_peer(name)?.ok_or_else(|| anyhow!("Unknown peer: {name}"))?;
            let result = send_message(peer, body).await?;
            ok(result);
            Ok(())
        }
        Some("inbox") => {
            ok(json!({ "messages": list_messages()? }));
            Ok(())
        }
        Some("read") => {
            let id = args.get(1).context("Usage: chatterp2p read <message-id>")?;
            let msg = list_messages()?
                .into_iter()
                .find(|m| &m.id == id)
                .ok_or_else(|| anyhow!("Message not found: {id}"))?;
            ok(json!({ "message": msg }));
            Ok(())
        }
        Some("daemon") => run_daemon(&args[1..]).await,
        Some("contact") if args.get(1).map(String::as_str) == Some("card") => {
            let (peer_id, _) = load_identity()?;
            let addrs = read_daemon_info().unwrap_or_default();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "peer_id": peer_id.to_string(),
                    "multiaddrs": addrs
                }))?
            );
            Ok(())
        }
        _ => bail!("Usage: chatterp2p <init|me|contact|peer|message|inbox|read|daemon>"),
    }
}

async fn run_peer(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("add") => {
            let peer_id = args
                .get(1)
                .context("Usage: chatterp2p peer add <peer-id> <name> <multiaddr...>")?;
            let name = args
                .get(2)
                .context("Usage: chatterp2p peer add <peer-id> <name> <multiaddr...>")?;
            if args.len() < 4 {
                bail!("Usage: chatterp2p peer add <peer-id> <name> <multiaddr...>");
            }
            let mut peer = None;
            for addr in &args[3..] {
                peer = Some(add_peer(peer_id, name, addr)?);
            }
            ok(json!({ "peer": peer }));
            Ok(())
        }
        Some("rm") => {
            let name = args
                .get(1)
                .context("Usage: chatterp2p peer rm <name-or-peer-id>")?;
            ok(json!({ "removed": remove_peer(name)? }));
            Ok(())
        }
        Some("list") => {
            ok(serde_json::to_value(load_peers()?)?);
            Ok(())
        }
        Some("show") => {
            let name = args
                .get(1)
                .context("Usage: chatterp2p peer show <name-or-peer-id>")?;
            let peer = find_peer(name)?.ok_or_else(|| anyhow!("Unknown peer: {name}"))?;
            ok(json!({ "peer": peer }));
            Ok(())
        }
        Some("import") => {
            let name = args
                .get(1)
                .context("Usage: chatterp2p peer import <name> <json-or-file>")?;
            let input = args[2..].join(" ");
            if input.is_empty() {
                bail!("Usage: chatterp2p peer import <name> <json-or-file>");
            }
            ok(json!({ "peer": import_peer_contact(&input, name)? }));
            Ok(())
        }
        _ => bail!("Usage: chatterp2p peer <add|rm|list|show|import>"),
    }
}

async fn run_daemon(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("start") => {
            parse_daemon_args(&args[1..])?;
            let status = daemon_status();
            if status["running"] == true {
                ok(status);
                return Ok(());
            }
            ensure_dirs()?;
            let exe = env::current_exe()?;
            let log_path = daemon_log_path();
            let log = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)?;
            let mut child_args = vec!["--daemon-child".to_string()];
            child_args.extend_from_slice(&args[1..]);
            let child = Command::new(exe)
                .args(child_args)
                .stdin(Stdio::null())
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log))
                .spawn()?;
            fs::write(daemon_pid_path(), format!("{}\n", child.id()))?;
            ok(json!({ "running": true, "pid": child.id(), "log": log_path }));
            Ok(())
        }
        Some("status") => {
            ok(daemon_status());
            Ok(())
        }
        Some("stop") => {
            let status = daemon_status();
            if status["running"] != true {
                ok(json!({ "stopped": false }));
                return Ok(());
            }
            let pid = status["pid"].as_u64().unwrap();
            #[cfg(unix)]
            {
                let _ = Command::new("kill")
                    .arg("-TERM")
                    .arg(pid.to_string())
                    .status();
            }
            let _ = fs::remove_file(daemon_pid_path());
            ok(json!({ "stopped": true, "pid": pid }));
            Ok(())
        }
        _ => bail!("Usage: chatterp2p daemon <start|status|stop>"),
    }
}

async fn run_receiver(args: &[String]) -> Result<()> {
    let opts = parse_daemon_args(args)?;
    let (peer_id, keypair) = load_or_create_identity()?;
    let mut swarm = build_swarm(keypair).await?;

    let cfg = load_config()?;
    for addr in opts
        .listen
        .iter()
        .chain(cfg.listen.iter())
        .take(if opts.listen.is_empty() {
            usize::MAX
        } else {
            opts.listen.len()
        })
    {
        Swarm::listen_on(&mut swarm, addr.parse()?)?;
    }
    for relay in &opts.relays {
        let addr: Multiaddr = relay.parse()?;
        let circuit = addr.with(Protocol::P2pCircuit);
        match Swarm::listen_on(&mut swarm, circuit.clone()) {
            Ok(_) => println!(
                "{}",
                json!({ "event": "relay_listen_started", "address": circuit.to_string() })
            ),
            Err(err) => println!(
                "{}",
                json!({ "event": "relay_listen_start_error", "address": circuit.to_string(), "error": err.to_string() })
            ),
        }
    }

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        let addrs: Vec<String> = swarm.external_addresses()
                            .map(|a| append_p2p(a.clone(), peer_id).to_string())
                            .chain(swarm.listeners().map(|a| append_p2p(a.clone(), peer_id).to_string()))
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&json!({
                            "success": true,
                            "mode": "daemon",
                            "peer_id": peer_id.to_string(),
                            "addresses": addrs,
                            "paths": {
                                "identity": identity_path(),
                                "peers": peers_path(),
                                "messages": messages_path(),
                                "log": daemon_log_path()
                            }
                        }))?);
                        let _ = std::io::stdout().flush();
                        let _ = address;
                    }
                    SwarmEvent::ListenerError { error, .. } => {
                        println!("{}", json!({ "event": "listener_error", "error": error.to_string() }));
                    }
                    SwarmEvent::OutgoingConnectionError { peer_id: failed_peer, error, .. } => {
                        println!("{}", json!({ "event": "outgoing_connection_error", "peer_id": failed_peer.map(|p| p.to_string()), "error": error.to_string() }));
                    }
                    SwarmEvent::IncomingConnectionError { error, .. } => {
                        println!("{}", json!({ "event": "incoming_connection_error", "error": error.to_string() }));
                    }
                    SwarmEvent::ConnectionEstablished { peer_id: connected_peer, endpoint, .. } => {
                        println!("{}", json!({ "event": "connection_established", "peer_id": connected_peer.to_string(), "endpoint": format!("{endpoint:?}") }));
                    }
                    SwarmEvent::ExternalAddrConfirmed { address } => {
                        println!("{}", json!({ "event": "external_addr_confirmed", "address": append_p2p(address, peer_id).to_string() }));
                    }
                    SwarmEvent::Behaviour(BehaviourEvent::Relay(event)) => {
                        println!("{}", json!({ "event": "relay_client", "detail": format!("{event:?}") }));
                    }
                    SwarmEvent::Behaviour(BehaviourEvent::Dm(request_response::Event::Message { peer, message, .. })) => {
                        if let request_response::Message::Request { request, channel, .. } = message {
                            if !request.body.is_empty() {
                                let msg = MessageFile {
                                    id: request.id,
                                    from: if request.from.is_empty() { peer.to_string() } else { request.from },
                                    to: request.to,
                                    sent_at: request.sent_at,
                                    received_at: now_iso(),
                                    body: request.body,
                                };
                                append_message(&msg)?;
                                println!("{}", json!({ "event": "message_received", "message": msg }));
                            }
                            let _ = swarm.behaviour_mut().dm.send_response(channel, DmResponse { ok: true });
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

async fn send_message(peer: PeerEntry, body: String) -> Result<serde_json::Value> {
    if body.as_bytes().len() > MAX_MESSAGE_BYTES {
        bail!("Message body exceeds {MAX_MESSAGE_BYTES} UTF-8 bytes.");
    }
    if peer.addresses.is_empty() {
        bail!("Peer has no known addresses: {}", peer.name);
    }

    let (our_peer_id, keypair) = load_identity()?;
    let mut swarm = build_swarm(keypair).await?;
    let target: PeerId = peer.peer_id.parse()?;
    let msg = DmRequest {
        id: message_id(),
        from: our_peer_id.to_string(),
        to: peer.peer_id.clone(),
        sent_at: now_iso(),
        body,
    };

    let mut pending = None;
    let mut dialed = None;
    let mut last_err = None;
    for addr in &peer.addresses {
        let maddr: Multiaddr = normalize_addr_for_peer(addr, target)?.parse()?;
        match Swarm::dial(&mut swarm, maddr.clone()) {
            Ok(_) => {
                pending = Some(swarm.behaviour_mut().dm.send_request(&target, msg.clone()));
                dialed = Some(maddr.to_string());
                break;
            }
            Err(err) => last_err = Some(err.to_string()),
        }
    }
    let request_id = pending.ok_or_else(|| {
        anyhow!(last_err.unwrap_or_else(|| "No dial attempts were made.".to_string()))
    })?;

    let deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => bail!("Timed out waiting for message acknowledgement."),
            event = swarm.select_next_some() => {
                if let SwarmEvent::Behaviour(BehaviourEvent::Dm(request_response::Event::Message { message, .. })) = event {
                    if let request_response::Message::Response { request_id: got, response } = message {
                        if got == request_id && response.ok {
                            return Ok(json!({ "message": msg, "dialed": dialed }));
                        }
                    }
                }
            }
        }
    }
}

async fn build_swarm(keypair: identity::Keypair) -> Result<Swarm<Behaviour>> {
    let peer_id = PeerId::from(keypair.public());
    let swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_websocket(noise::Config::new, yamux::Config::default)
        .await?
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(move |key, relay_behaviour| {
            let protocols = std::iter::once((
                StreamProtocol::new(DM_PROTOCOL),
                request_response::ProtocolSupport::Full,
            ));
            Behaviour {
                identify: identify::Behaviour::new(identify::Config::new(
                    "chatterp2p/0.0.1".to_string(),
                    key.public(),
                )),
                relay: relay_behaviour,
                dm: request_response::json::Behaviour::new(
                    protocols,
                    request_response::Config::default(),
                ),
            }
        })?
        .build();
    let _ = peer_id;
    Ok(swarm)
}

fn parse_daemon_args(args: &[String]) -> Result<DaemonOptions> {
    let mut listen = Vec::new();
    let mut relays = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--listen" => {
                i += 1;
                let addr = args.get(i).context("Usage: chatterp2p daemon start [--listen <multiaddr>] [--relay <relay-multiaddr>]")?;
                listen.push(addr.clone());
            }
            "--relay" => {
                i += 1;
                let addr = args.get(i).context("Usage: chatterp2p daemon start [--listen <multiaddr>] [--relay <relay-multiaddr>]")?;
                relays.push(addr.clone());
            }
            arg if arg.starts_with("--") => bail!("Unknown option: {arg}"),
            arg => bail!("Unknown argument: {arg}"),
        }
        i += 1;
    }
    Ok(DaemonOptions { listen, relays })
}

fn ok(value: serde_json::Value) {
    let mut map = serde_json::Map::new();
    map.insert("success".to_string(), serde_json::Value::Bool(true));
    if let serde_json::Value::Object(obj) = value {
        map.extend(obj);
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::Value::Object(map)).unwrap()
    );
}

fn config_dir() -> PathBuf {
    env::var_os("CHATTERP2P_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("AGENTCHAT_CONFIG_DIR").map(PathBuf::from))
        .unwrap_or_else(|| home_dir().join(".config/chatterp2p"))
}

fn data_dir() -> PathBuf {
    env::var_os("CHATTERP2P_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("AGENTCHAT_DATA_DIR").map(PathBuf::from))
        .unwrap_or_else(|| home_dir().join(".local/share/chatterp2p"))
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn identity_path() -> PathBuf {
    config_dir().join("identity.json")
}
fn config_path() -> PathBuf {
    config_dir().join("config.json")
}
fn peers_path() -> PathBuf {
    config_dir().join("peers.json")
}
fn messages_path() -> PathBuf {
    data_dir().join("messages.jsonl")
}
fn daemon_pid_path() -> PathBuf {
    data_dir().join("daemon.pid")
}
fn daemon_log_path() -> PathBuf {
    data_dir().join("daemon.log")
}

fn ensure_dirs() -> Result<()> {
    fs::create_dir_all(config_dir())?;
    fs::create_dir_all(data_dir())?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match fs::read_to_string(path) {
        Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    ensure_dirs()?;
    fs::write(path, format!("{}\n", serde_json::to_string_pretty(value)?))?;
    Ok(())
}

fn load_or_create_identity() -> Result<(PeerId, identity::Keypair)> {
    if identity_path().exists() {
        return load_identity();
    }
    ensure_dirs()?;
    let key = identity::Keypair::generate_ed25519();
    let peer_id = PeerId::from(key.public());
    let encoded = B64.encode(key.to_protobuf_encoding()?);
    write_json(
        &identity_path(),
        &IdentityFile {
            kind: "Ed25519".to_string(),
            private_key_protobuf_base64: encoded,
            peer_id: peer_id.to_string(),
            created_at: now_iso(),
        },
    )?;
    let _ = load_config()?;
    let _ = load_peers()?;
    Ok((peer_id, key))
}

fn load_identity() -> Result<(PeerId, identity::Keypair)> {
    let saved: IdentityFile = read_json(&identity_path())?
        .ok_or_else(|| anyhow!("Identity not initialized. Run `chatterp2p init` first."))?;
    let bytes = B64.decode(saved.private_key_protobuf_base64)?;
    let key = identity::Keypair::from_protobuf_encoding(&bytes)?;
    let peer_id = PeerId::from(key.public());
    Ok((peer_id, key))
}

fn load_config() -> Result<ConfigFile> {
    if let Some(cfg) = read_json(&config_path())? {
        return Ok(cfg);
    }
    let cfg = ConfigFile {
        listen: vec!["/ip4/0.0.0.0/tcp/0/ws".to_string()],
    };
    save_config(&cfg)?;
    Ok(cfg)
}

fn save_config(cfg: &ConfigFile) -> Result<()> {
    write_json(&config_path(), cfg)
}

fn load_peers() -> Result<PeerBook> {
    if let Some(book) = read_json(&peers_path())? {
        return Ok(book);
    }
    let book = PeerBook { peers: vec![] };
    save_peers(&book)?;
    Ok(book)
}

fn save_peers(book: &PeerBook) -> Result<()> {
    write_json(&peers_path(), book)
}

fn add_peer(peer_id_text: &str, name: &str, addr: &str) -> Result<PeerEntry> {
    let peer_id: PeerId = peer_id_text.parse()?;
    if !valid_name(name) {
        bail!("Peer name must be 1-64 chars using letters, numbers, dot, underscore, or hyphen.");
    }
    let _: Multiaddr = addr.parse()?;
    let mut book = load_peers()?;
    if book
        .peers
        .iter()
        .any(|p| p.name == name && p.peer_id != peer_id.to_string())
    {
        bail!("Peer name already exists for a different Peer ID: {name}");
    }
    let now = now_iso();
    if let Some(existing) = book
        .peers
        .iter_mut()
        .find(|p| p.peer_id == peer_id.to_string() || p.name == name)
    {
        existing.peer_id = peer_id.to_string();
        existing.name = name.to_string();
        if !existing.addresses.iter().any(|a| a == addr) {
            existing.addresses.push(addr.to_string());
        }
        existing.updated_at = now;
    } else {
        book.peers.push(PeerEntry {
            peer_id: peer_id.to_string(),
            name: name.to_string(),
            addresses: vec![addr.to_string()],
            created_at: now.clone(),
            updated_at: now,
        });
    }
    let peer = book
        .peers
        .iter()
        .find(|p| p.peer_id == peer_id.to_string())
        .unwrap()
        .clone();
    save_peers(&book)?;
    Ok(peer)
}

fn remove_peer(name_or_peer_id: &str) -> Result<usize> {
    let mut book = load_peers()?;
    let before = book.peers.len();
    book.peers
        .retain(|p| p.name != name_or_peer_id && p.peer_id != name_or_peer_id);
    let removed = before - book.peers.len();
    save_peers(&book)?;
    Ok(removed)
}

fn find_peer(name_or_peer_id: &str) -> Result<Option<PeerEntry>> {
    Ok(load_peers()?
        .peers
        .into_iter()
        .find(|p| p.name == name_or_peer_id || p.peer_id == name_or_peer_id))
}

fn import_peer_contact(input: &str, local_name: &str) -> Result<PeerEntry> {
    let raw = if input.trim_start().starts_with('{') {
        input.to_string()
    } else {
        fs::read_to_string(input)?
    };
    let value: serde_json::Value = serde_json::from_str(&raw)?;
    let card_value = value.get("chatterp2p").unwrap_or(&value).clone();
    let card: ContactCard = serde_json::from_value(card_value)?;
    let mut addrs = Vec::new();
    if let Some(addr) = card.multiaddr {
        addrs.push(addr);
    }
    addrs.extend(card.multiaddrs);
    addrs.extend(card.direct_addresses);
    addrs.extend(card.relay_addresses);
    if addrs.is_empty() {
        bail!("Contact card must include at least one multiaddr.");
    }
    let mut peer = None;
    for addr in addrs {
        peer = Some(add_peer(&card.peer_id, local_name, &addr)?);
    }
    Ok(peer.unwrap())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn append_message(message: &MessageFile) -> Result<()> {
    ensure_dirs()?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(messages_path())?;
    writeln!(f, "{}", serde_json::to_string(message)?)?;
    Ok(())
}

fn list_messages() -> Result<Vec<MessageFile>> {
    match fs::read_to_string(messages_path()) {
        Ok(raw) => raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| Ok(serde_json::from_str(l)?))
            .collect(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(err) => Err(err.into()),
    }
}

fn daemon_status() -> serde_json::Value {
    let Ok(raw) = fs::read_to_string(daemon_pid_path()) else {
        return json!({ "running": false });
    };
    let Ok(pid) = raw.trim().parse::<u32>() else {
        return json!({ "running": false });
    };
    #[cfg(unix)]
    let running = Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    #[cfg(not(unix))]
    let running = true;
    if running {
        json!({ "running": true, "pid": pid, "log": daemon_log_path() })
    } else {
        json!({ "running": false })
    }
}

fn read_daemon_info() -> Result<Vec<String>> {
    let raw = fs::read_to_string(daemon_log_path())?;
    let mut latest = None;
    let mut acc = String::new();
    for line in raw.lines() {
        if line.trim().is_empty() && acc.is_empty() {
            continue;
        }
        acc.push_str(line);
        acc.push('\n');
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&acc) {
            if value.get("success") == Some(&serde_json::Value::Bool(true)) {
                if let Some(addrs) = value.get("addresses").and_then(|v| v.as_array()) {
                    latest = Some(
                        addrs
                            .iter()
                            .filter_map(|v| v.as_str().map(ToString::to_string))
                            .collect(),
                    );
                }
            }
            acc.clear();
        }
    }
    Ok(latest.unwrap_or_default())
}

fn append_p2p(mut addr: Multiaddr, peer_id: PeerId) -> Multiaddr {
    if !addr.iter().any(|p| matches!(p, Protocol::P2p(_))) {
        addr.push(Protocol::P2p(peer_id));
    }
    addr
}

fn normalize_addr_for_peer(addr: &str, peer_id: PeerId) -> Result<String> {
    let parsed: Multiaddr = addr.parse()?;
    if parsed.iter().any(|p| matches!(p, Protocol::P2p(_))) {
        Ok(addr.to_string())
    } else {
        let mut with_peer = parsed;
        with_peer.push(Protocol::P2p(peer_id));
        Ok(with_peer.to_string())
    }
}

fn message_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("msg_{:x}", nanos)
}

fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let days = (secs / 86_400) as i64;
    let sec_of_day = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = sec_of_day / 3_600;
    let minute = (sec_of_day % 3_600) / 60;
    let second = sec_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year, m as u32, d as u32)
}

fn print_usage() {
    println!(
        r#"chatterp2p {VERSION}

Usage:
  chatterp2p --help
  chatterp2p --version
  chatterp2p init
  chatterp2p me
  chatterp2p contact card
  chatterp2p peer add <peer-id> <name> <multiaddr...>
  chatterp2p peer import <name> <json-or-file>
  chatterp2p peer list
  chatterp2p peer show <name-or-peer-id>
  chatterp2p peer rm <name-or-peer-id>
  chatterp2p message <name-or-peer-id> <message>
  chatterp2p inbox
  chatterp2p read <message-id>
  chatterp2p daemon start [--listen <multiaddr>] [--relay <relay-multiaddr>]
  chatterp2p daemon status
  chatterp2p daemon stop"#
    );
}
