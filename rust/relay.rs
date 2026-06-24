use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use futures::StreamExt;
use libp2p::{
    Multiaddr, PeerId, Swarm, SwarmBuilder, identify, identity, multiaddr::Protocol, noise, relay,
    swarm::SwarmEvent, yamux,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const VERSION: &str = "0.0.1";

#[derive(libp2p::swarm::NetworkBehaviour)]
struct RelayBehaviour {
    identify: identify::Behaviour,
    relay: relay::Behaviour,
}

#[derive(Debug, Deserialize, Serialize)]
struct IdentityFile {
    #[serde(rename = "type")]
    kind: String,
    private_key_protobuf_base64: String,
    peer_id: String,
    created_at: String,
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
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return Ok(());
    }
    if args.iter().any(|a| a == "--version" || a == "-v") {
        println!("{VERSION}");
        return Ok(());
    }

    let opts = parse_args(&args)?;
    let (peer_id, keypair) = load_or_create_identity(opts.identity.as_deref())?;
    let mut swarm = build_swarm(keypair).await?;

    for addr in &opts.listen {
        Swarm::listen_on(&mut swarm, addr.parse()?)?;
    }

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        swarm.add_external_address(address.clone());
                        let addrs: Vec<String> = swarm.listeners()
                            .map(|a| append_p2p(a.clone(), peer_id).to_string())
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&json!({
                            "success": true,
                            "mode": "relay",
                            "peer_id": peer_id.to_string(),
                            "addresses": addrs,
                            "identity": identity_path(opts.identity.as_deref())
                        }))?);
                        let _ = address;
                    }
                    SwarmEvent::Behaviour(RelayBehaviourEvent::Relay(event)) => {
                        println!("{}", json!({ "event": "relay", "detail": format!("{event:?}") }));
                    }
                    SwarmEvent::Behaviour(RelayBehaviourEvent::Identify(_)) => {}
                    SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                        println!("{}", json!({ "event": "connection_established", "peer_id": peer_id.to_string(), "endpoint": format!("{endpoint:?}") }));
                    }
                    SwarmEvent::ConnectionClosed { peer_id, .. } => {
                        println!("{}", json!({ "event": "connection_closed", "peer_id": peer_id.to_string() }));
                    }
                    SwarmEvent::IncomingConnectionError { error, .. } => {
                        println!("{}", json!({ "event": "incoming_connection_error", "error": error.to_string() }));
                    }
                    SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                        println!("{}", json!({ "event": "outgoing_connection_error", "peer_id": peer_id.map(|p| p.to_string()), "error": error.to_string() }));
                    }
                    SwarmEvent::ListenerError { error, .. } => {
                        println!("{}", json!({ "event": "listener_error", "error": error.to_string() }));
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

struct Options {
    listen: Vec<String>,
    identity: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<Options> {
    let mut listen = vec!["/ip4/0.0.0.0/tcp/4001/ws".to_string()];
    let mut identity = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--listen" => {
                i += 1;
                let addr = args.get(i).context(
                    "Usage: chatterp2p-relay [--listen <multiaddr>] [--identity <path>]",
                )?;
                listen = vec![addr.clone()];
            }
            "--identity" => {
                i += 1;
                let path = args.get(i).context(
                    "Usage: chatterp2p-relay [--listen <multiaddr>] [--identity <path>]",
                )?;
                identity = Some(PathBuf::from(path));
            }
            arg if arg.starts_with("--") => bail!("Unknown option: {arg}"),
            arg => bail!("Unknown argument: {arg}"),
        }
        i += 1;
    }
    Ok(Options { listen, identity })
}

async fn build_swarm(keypair: identity::Keypair) -> Result<Swarm<RelayBehaviour>> {
    let local_peer_id = PeerId::from(keypair.public());
    let swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_websocket(noise::Config::new, yamux::Config::default)
        .await?
        .with_behaviour(move |key| RelayBehaviour {
            identify: identify::Behaviour::new(identify::Config::new(
                "chatterp2p-relay/0.0.1".to_string(),
                key.public(),
            )),
            relay: relay::Behaviour::new(local_peer_id, relay::Config::default()),
        })?
        .build();
    Ok(swarm)
}

fn append_p2p(mut addr: Multiaddr, peer_id: PeerId) -> Multiaddr {
    if !addr.iter().any(|p| matches!(p, Protocol::P2p(_))) {
        addr.push(Protocol::P2p(peer_id));
    }
    addr
}

fn identity_path(override_path: Option<&Path>) -> PathBuf {
    override_path.map(PathBuf::from).unwrap_or_else(|| {
        env::var_os("CHATTERP2P_RELAY_IDENTITY")
            .map(PathBuf::from)
            .unwrap_or_else(|| config_dir().join("identity.json"))
    })
}

fn config_dir() -> PathBuf {
    env::var_os("CHATTERP2P_RELAY_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config/chatterp2p-relay"))
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn load_or_create_identity(override_path: Option<&Path>) -> Result<(PeerId, identity::Keypair)> {
    let path = identity_path(override_path);
    if path.exists() {
        let saved: IdentityFile = serde_json::from_str(&fs::read_to_string(&path)?)?;
        let bytes = B64.decode(saved.private_key_protobuf_base64)?;
        let key = identity::Keypair::from_protobuf_encoding(&bytes)?;
        let peer_id = PeerId::from(key.public());
        return Ok((peer_id, key));
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let key = identity::Keypair::generate_ed25519();
    let peer_id = PeerId::from(key.public());
    fs::write(
        &path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&IdentityFile {
                kind: "Ed25519".to_string(),
                private_key_protobuf_base64: B64.encode(key.to_protobuf_encoding()?),
                peer_id: peer_id.to_string(),
                created_at: now_iso(),
            })?
        ),
    )?;
    Ok((peer_id, key))
}

fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{secs}")
}

fn print_usage() {
    println!(
        r#"chatterp2p-relay {VERSION}

Usage:
  chatterp2p-relay --help
  chatterp2p-relay --version
  chatterp2p-relay [--listen <multiaddr>] [--identity <path>]

Default listen:
  /ip4/0.0.0.0/tcp/4001/ws"#
    );
}
