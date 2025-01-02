use crate::mod_libp2p::behavior::AgentBehavior;
use base64::{engine::general_purpose::STANDARD, Engine};
use either::Either;
use libp2p::{
    core::transport::upgrade::Version,
    identify::{Behaviour as IdentifyBehavior, Config as IdentifyConfig},
    identity::{self, Keypair},
    kad,
    kad::{store::MemoryStore as KadInMemory, Behaviour as KadBehavior, Config as KadConfig},
    multiaddr::Protocol,
    noise, ping,
    pnet::{PnetConfig, PreSharedKey},
    tcp, yamux, Multiaddr, PeerId, StreamProtocol, Swarm, Transport,
};
use std::{error::Error, str::FromStr, time::Duration};
use tracing::info;

pub mod behavior;
pub mod network;

const BOOT_ADDRESS: &str = "/ip4/192.168.1.136/tcp/8002";

pub async fn start_swarm() -> Result<(Swarm<AgentBehavior>, Keypair), Box<dyn Error>> {
    let sk = std::env::var("ACCOUNT_SK").expect("ACCOUNT_SK missing in .env");
    let private_key_bytes = hex::decode(sk)?;
    let secret_key = identity::secp256k1::SecretKey::try_from_bytes(private_key_bytes)?;
    let libp2p_keypair: Keypair = identity::secp256k1::Keypair::from(secret_key).into();

    let psk = get_psk();

    if let Ok(psk) = psk {
        info!("using swarm key with fingerprint: {}", psk.fingerprint());
    }

    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(libp2p_keypair.clone())
        .with_tokio()
        .with_other_transport(|key| {
            let noise_config = noise::Config::new(key).unwrap();
            let mut yamux_config = yamux::Config::default();
            yamux_config.set_max_num_streams(1024 * 1024);
            let base_transport = tcp::tokio::Transport::new(tcp::Config::default().nodelay(true));
            let maybe_encrypted = match psk {
                Ok(psk) => Either::Left(
                    base_transport
                        .and_then(move |socket, _| PnetConfig::new(psk).handshake(socket)),
                ),
                Err(_) => Either::Right(base_transport),
            };
            maybe_encrypted
                .upgrade(Version::V1Lazy)
                .authenticate(noise_config)
                .multiplex(yamux_config)
        })?
        .with_dns()?
        .with_behaviour(|key| {
            let local_peer_id = PeerId::from(key.clone().public());

            let mut kad_config = KadConfig::new(StreamProtocol::new("/agent/connection/1.0.0"));
            kad_config.set_periodic_bootstrap_interval(Some(Duration::from_secs(10)));
            kad_config.set_publication_interval(Some(Duration::from_secs(120)));
            kad_config.set_replication_interval(Some(Duration::from_secs(120)));
            kad_config.set_periodic_bootstrap_interval(Some(Duration::from_secs(300)));
            let kad_memory = KadInMemory::new(local_peer_id);
            let kad = KadBehavior::with_config(local_peer_id, kad_memory, kad_config);

            let identify_config =
                IdentifyConfig::new("/agent/connection/1.0.0".to_string(), key.clone().public())
                    .with_push_listen_addr_updates(true)
                    .with_interval(Duration::from_secs(30));
            let identify = IdentifyBehavior::new(identify_config);

            let ping =
                ping::Behaviour::new(ping::Config::new().with_interval(Duration::from_secs(10)));

            AgentBehavior::new(kad, identify, ping)
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    let addr: Multiaddr = parse_legacy_multiaddr(&BOOT_ADDRESS)?;

    let _ = swarm.dial(addr)?;

    swarm.behaviour_mut().kad.set_mode(Some(kad::Mode::Server));

    let private_net_address =
        std::env::var("PRIVITE_NET_ADDRESS").unwrap_or("/ip4/0.0.0.0/tcp/8000".to_string());
    let private_net_address = private_net_address.parse()?;
    swarm.listen_on(private_net_address)?;
    Ok((swarm, libp2p_keypair))
}

/// Read the pre shared key file from the given ipfs directory
fn get_psk() -> Result<PreSharedKey, Box<dyn Error>> {
    let base64_key =
        std::env::var("PRIVITE_NET_KEY").map_err(|_| "PRIVITE_NET_KEY missing in .env")?;
    let bytes = STANDARD.decode(&base64_key)?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "Decoded key must be 32 bytes long")?;
    Ok(PreSharedKey::new(key))
}

/// for a multiaddr that ends with a peer id, this strips this suffix. Rust-libp2p
/// only supports dialing to an address without providing the peer id.
fn strip_peer_id(addr: &mut Multiaddr) {
    let last = addr.pop();
    match last {
        Some(Protocol::P2p(peer_id)) => {
            let mut addr = Multiaddr::empty();
            addr.push(Protocol::P2p(peer_id));
            // info!("removing peer id {addr} so this address can be dialed by rust-libp2p");
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
