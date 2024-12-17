use crate::mod_libp2p::message::GreetRequest;
use crate::mod_libp2p::{
    behavior::{AgentBehavior, AgentEvent},
    message::{AgentMessage, GreetResponse},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use either::Either;
use libp2p::{
    core::transport::upgrade::Version,
    futures::StreamExt,
    gossipsub::{self, Event as GossipsubEvent},
    identify::{Behaviour as IdentifyBehavior, Config as IdentifyConfig},
    identity::{self, Keypair},
    kad::{store::MemoryStore as KadInMemory, Behaviour as KadBehavior, Config as KadConfig},
    mdns,
    multiaddr::Protocol,
    noise, ping,
    pnet::{PnetConfig, PreSharedKey},
    request_response::{
        json::Behaviour as RequestResponseBehavior, Config as RequestResponseConfig,
        Event as RequestResponseEvent, Message as RequestResponseMessage,
        ProtocolSupport as RequestResponseProtocolSupport,
    },
    swarm::SwarmEvent,
    tcp, yamux, Multiaddr, PeerId, StreamProtocol, Swarm, Transport,
};
use std::{
    collections::hash_map::DefaultHasher,
    env,
    error::Error,
    hash::{Hash, Hasher},
    path::Path,
    str::FromStr,
    time::Duration,
};
use tokio::time;
use tracing::error;
use tracing::info;
use tracing::warn;

pub mod behavior;
pub mod message;

const BOOTNODES: [&str; 1] = ["16Uiu2HAmLiJHsiwFyVEXnN6QvdH1eVBrsaTNdPqA6xxJbTf1bMbz"];

const TESTNET_ADDRESS: [&str; 1] = ["/ip4/192.168.1.136/tcp/8003"];

pub async fn start_swarm() -> Result<(Swarm<AgentBehavior>, Keypair), Box<dyn Error>> {
    let sk = std::env::var("ACCOUNT_SK").expect("ACCOUNT_SK missing in .env");
    let private_key_bytes = hex::decode(sk)?;
    let secret_key = identity::secp256k1::SecretKey::try_from_bytes(private_key_bytes)?;
    let libp2p_keypair: Keypair = identity::secp256k1::Keypair::from(secret_key).into();

    let psk = get_psk();

    if let Ok(psk) = psk {
        info!("using swarm key with fingerprint: {}", psk.fingerprint());
    }

    // Create a Gosspipsub topic
    let gossipsub_topic = gossipsub::IdentTopic::new("chat");

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

            let kad_config = KadConfig::new(StreamProtocol::new("/agent/connection/1.0.0"));
            let kad_memory = KadInMemory::new(local_peer_id);
            let kad = KadBehavior::with_config(local_peer_id, kad_memory, kad_config);

            let rr_config =
                RequestResponseConfig::default().with_max_concurrent_streams(1024 * 1024);
            let rr_protocol = StreamProtocol::new("/agent/message/1.0.0");
            let rr_behavior = RequestResponseBehavior::<AgentMessage, AgentMessage>::new(
                [(rr_protocol, RequestResponseProtocolSupport::Full)],
                rr_config,
            );

            let identify_config =
                IdentifyConfig::new("/agent/connection/1.0.0".to_string(), key.clone().public())
                    .with_push_listen_addr_updates(true)
                    .with_interval(Duration::from_secs(30));
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
                ping::Behaviour::new(ping::Config::new().with_interval(Duration::from_secs(10)));
            let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id).unwrap();
            AgentBehavior::new(kad, identify, rr_behavior, gossipsub, ping)
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    for (peer, addr) in BOOTNODES.iter().zip(TESTNET_ADDRESS.iter()) {
        let peer_id: PeerId = peer.parse()?;
        let multiaddr: Multiaddr = addr.parse()?;
        swarm.behaviour_mut().kad.add_address(&peer_id, multiaddr);
    }

    let mut multiaddr_list: Vec<Multiaddr> = vec![];
    for to_dial in TESTNET_ADDRESS {
        let addr: Multiaddr = parse_legacy_multiaddr(&to_dial)?;
        multiaddr_list.push(addr.clone());
        let _ = swarm.dial(addr)?;
    }

    swarm
        .behaviour_mut()
        .gossipsub
        .subscribe(&gossipsub_topic)
        .unwrap();

    let private_net_address =
        std::env::var("PRIVITE_NET_ADDRESS").unwrap_or("/ip4/0.0.0.0/tcp/8000".to_string());
    let private_net_address = private_net_address.parse()?;
    swarm.listen_on(private_net_address)?;
    Ok((swarm, libp2p_keypair))
}

pub async fn handle_swarm_event(mut swarm: Swarm<AgentBehavior>, local_key: Keypair) {
    let mut interval1 = time::interval(Duration::from_secs(8));
    let mut peer_list = vec![];
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(event) = swarm.next() => {
                    warn!("event is {:?}", event);
                    handle_event(&mut swarm, event, local_key.clone(), &mut peer_list).await;
                }
                _ = interval1.tick() => {
                    let local_peer_id = local_key.public().to_peer_id();
                    let request = GreetRequest {
                        message: format!("Send message from: {local_peer_id}:hello gause from network relay -----------------------"),
                    };
                    let request_message = AgentMessage::GreetRequest(request);
                    for peer_id in &peer_list {
                        let _request_id = swarm
                            .behaviour_mut()
                            .send_message(peer_id, request_message.clone());
                    }
                    interval1 = time::interval(Duration::from_secs(8));
                    interval1.reset();
                }
            }
        }
    });
}

async fn handle_event(
    swarm: &mut Swarm<AgentBehavior>,
    swarm_event: SwarmEvent<AgentEvent>,
    local_key: Keypair,
    peer_list: &mut Vec<PeerId>,
) {
    match swarm_event {
        SwarmEvent::ConnectionEstablished { peer_id, .. } => peer_list.push(peer_id),
        SwarmEvent::Behaviour(AgentEvent::RequestResponse(RequestResponseEvent::Message {
            peer,
            message,
        })) => match message {
            RequestResponseMessage::Request {
                request_id,
                request,
                channel,
            } => {
                // let parsed_request =
                //     AgentMessage::from_binary(&request).expect("Failed to decode request");
                match request {
                    AgentMessage::GreetRequest(..) => {}
                    AgentMessage::AnotherMessage(..) => {}
                    _ => {
                        info!("Received unknown message type.");
                    }
                }
                let local_peer_id = local_key.public().to_peer_id();
                let response = GreetResponse {
                    message: format!("Response from: {local_peer_id}: hello too").to_string(),
                };
                let response_message = AgentMessage::GreetResponse(response);
                let result = swarm
                    .behaviour_mut()
                    .send_response(channel, response_message);
                if result.is_err() {
                    let err = result.unwrap_err();
                    error!("Error sending response: {err:?}")
                }
            }
            RequestResponseMessage::Response {
                request_id,
                response,
            } => {
                // let parsed_response =
                //     AgentMessage::from_binary(&response).expect("Failed to decode response");
                match response {
                    AgentMessage::GreetResponse(..) => {}
                    _ => {
                        info!("Received unknown response type.");
                    }
                }
            }
        },
        // Behaviour(Gossipsub(Message { propagation_source: PeerId("16Uiu2HAmDDTVeUo5tUM58BP5YyyJGfVo6YaKwFXkf4UKydbZofDS"), message_id: MessageId(3134393231373432343733363435313537303031), message: Message { data: 7b2274797065223a22.., source: Some(PeerId("16Uiu2HAmDDTVeUo5tUM58BP5YyyJGfVo6YaKwFXkf4UKydbZofDS")), sequence_number: Some(1733299423449580817), topic: TopicHash { hash: "chat" } } }))
        SwarmEvent::Behaviour(AgentEvent::Gossipsub(event)) => match event {
            GossipsubEvent::Message { message, .. } => {
                match AgentMessage::from_binary(&message.data) {
                    Ok(..) => {
                        // info!("broadcast agent message is {:#?}", agent_message);
                    }
                    _ => {}
                }
            }
            _ => {}
        },
        _ => {}
    }
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
