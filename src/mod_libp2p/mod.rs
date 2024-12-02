use crate::mod_libp2p::behavior::AgentBehavior;
use either::Either;
use libp2p::{
    core::transport::upgrade::Version,
    futures::StreamExt,
    gossipsub,
    identify::{Behaviour as IdentifyBehavior, Config as IdentifyConfig},
    kad::{store::MemoryStore as KadInMemory, Behaviour as KadBehavior, Config as KadConfig},
    multiaddr::Protocol,
    noise, ping,
    pnet::{PnetConfig, PreSharedKey},
    request_response::{
        cbor::Behaviour as RequestResponseBehavior, Config as RequestResponseConfig,
        ProtocolSupport as RequestResponseProtocolSupport,
    },
    tcp, yamux, Multiaddr, PeerId, StreamProtocol, Swarm, Transport,
};
use std::{
    collections::hash_map::DefaultHasher,
    env,
    error::Error,
    fs,
    hash::{Hash, Hasher},
    path::Path,
    str::FromStr,
    time::Duration,
};
use tracing::info;

pub mod behavior;
pub mod message;

pub async fn start_swarm() -> Result<Swarm<AgentBehavior>, Box<dyn Error>> {
    let ipfs_path = get_ipfs_path();
    println!("using IPFS_PATH {ipfs_path:?}");
    let psk: Option<PreSharedKey> = get_psk(&ipfs_path)?
        .map(|text| PreSharedKey::from_str(&text))
        .transpose()?;

    if let Some(psk) = psk {
        println!("using swarm key with fingerprint: {}", psk.fingerprint());
    }

    // Create a Gosspipsub topic
    let gossipsub_topic = gossipsub::IdentTopic::new("chat");

    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_other_transport(|key| {
            let noise_config = noise::Config::new(key).unwrap();
            let yamux_config = yamux::Config::default();

            let base_transport = tcp::tokio::Transport::new(tcp::Config::default().nodelay(true));
            let maybe_encrypted = match psk {
                Some(psk) => Either::Left(
                    base_transport
                        .and_then(move |socket, _| PnetConfig::new(psk).handshake(socket)),
                ),
                None => Either::Right(base_transport),
            };
            maybe_encrypted
                .upgrade(Version::V1Lazy)
                .authenticate(noise_config)
                .multiplex(yamux_config)
        })?
        .with_dns()?
        .with_behaviour(|key| {
            let local_peer_id = PeerId::from(key.clone().public());
            info!("LocalPeerID: {local_peer_id}");

            let kad_config = KadConfig::new(StreamProtocol::new("/agent/connection/1.0.0"));

            let kad_memory = KadInMemory::new(local_peer_id);
            let kad = KadBehavior::with_config(local_peer_id, kad_memory, kad_config);

            let identify_config =
                IdentifyConfig::new("/agent/connection/1.0.0".to_string(), key.clone().public())
                    .with_push_listen_addr_updates(true)
                    .with_interval(Duration::from_secs(30));

            let rr_config = RequestResponseConfig::default();
            let rr_protocol = StreamProtocol::new("/agent/message/1.0.0");
            let rr_behavior = RequestResponseBehavior::<Vec<u8>, Vec<u8>>::new(
                [(rr_protocol, RequestResponseProtocolSupport::Full)],
                rr_config,
            );

            let identify = IdentifyBehavior::new(identify_config);

            let message_id_fn = |message: &gossipsub::Message| {
                let mut s = DefaultHasher::new();
                message.data.hash(&mut s);
                gossipsub::MessageId::from(s.finish().to_string())
            };
            // Set a custom gossipsub configuration
            let gossipsub_config = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(10)) // This is set to aid debugging by not cluttering the log space
                .validation_mode(gossipsub::ValidationMode::Strict) // This sets the kind of message validation. The default is Strict (enforce message signing)
                .message_id_fn(message_id_fn) // content-address messages. No two messages of the same content will be propagated.
                .build()
                .unwrap();

            // build a gossipsub network behaviour
            let gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
            )
            .unwrap();
            let ping =
                ping::Behaviour::new(ping::Config::new().with_interval(Duration::from_secs(1)));
            AgentBehavior::new(kad, identify, rr_behavior, gossipsub, ping)
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    println!("Subscribing to {gossipsub_topic:?}");
    swarm
        .behaviour_mut()
        .gossipsub
        .subscribe(&gossipsub_topic)
        .unwrap();

    Ok(swarm)
}

pub async fn handle_swarm_event(mut swarm: Swarm<AgentBehavior>) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                event = swarm.next() => {
                    println!("event is {:?}", event);
                }
            }
        }
    });
}

/// Get the current ipfs repo path, either from the IPFS_PATH environment variable or
/// from the default $HOME/.ipfs
fn get_ipfs_path() -> Box<Path> {
    env::var("IPFS_PATH")
        .map(|ipfs_path| Path::new(&ipfs_path).into())
        .unwrap_or_else(|_| {
            env::var("HOME")
                .map(|home| Path::new(&home).join(".ipfs"))
                .expect("could not determine home directory")
                .into()
        })
}

/// Read the pre shared key file from the given ipfs directory
fn get_psk(path: &Path) -> std::io::Result<Option<String>> {
    let swarm_key_file = path.join("swarm.key");
    match fs::read_to_string(swarm_key_file) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// for a multiaddr that ends with a peer id, this strips this suffix. Rust-libp2p
/// only supports dialing to an address without providing the peer id.
fn strip_peer_id(addr: &mut Multiaddr) {
    let last = addr.pop();
    match last {
        Some(Protocol::P2p(peer_id)) => {
            let mut addr = Multiaddr::empty();
            addr.push(Protocol::P2p(peer_id));
            println!("removing peer id {addr} so this address can be dialed by rust-libp2p");
        }
        Some(other) => addr.push(other),
        _ => {}
    }
}

/// parse a legacy multiaddr (replace ipfs with p2p), and strip the peer id
/// so it can be dialed by rust-libp2p
fn parse_legacy_multiaddr(text: &str) -> Result<Multiaddr, Box<dyn Error>> {
    let sanitized = text
        .split('/')
        .map(|part| if part == "ipfs" { "p2p" } else { part })
        .collect::<Vec<_>>()
        .join("/");
    let mut res = Multiaddr::from_str(&sanitized)?;
    strip_peer_id(&mut res);
    Ok(res)
}