use crate::mod_libp2p::{
    behavior::{AgentBehavior, AgentEvent},
    message::{AgentMessage, GreetResponse},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use either::Either;
use libp2p::{
    core::transport::upgrade::Version,
    futures::StreamExt,
    gossipsub,
    identify::{Behaviour as IdentifyBehavior, Config as IdentifyConfig},
    identity::{self, Keypair},
    kad::{store::MemoryStore as KadInMemory, Behaviour as KadBehavior, Config as KadConfig},
    multiaddr::Protocol,
    noise, ping,
    pnet::{PnetConfig, PreSharedKey},
    request_response::{
        cbor::Behaviour as RequestResponseBehavior, Config as RequestResponseConfig,
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
use tracing::error;
use tracing::info;
use tracing::warn;

pub mod behavior;
pub mod message;

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
            let yamux_config = yamux::Config::default();

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
                ping::Behaviour::new(ping::Config::new().with_interval(Duration::from_secs(10)));
            AgentBehavior::new(kad, identify, rr_behavior, gossipsub, ping)
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

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
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(event) = swarm.next() => {
                    info!("event is {:?}", event);
                    handle_event(&mut swarm, event, local_key.clone()).await;
                }
            }
        }
    });
}

async fn handle_event(
    swarm: &mut Swarm<AgentBehavior>,
    swarm_event: SwarmEvent<AgentEvent>,
    local_key: Keypair,
) {
    info!("swarm_event is {:?}", swarm_event);
    match swarm_event {
        SwarmEvent::Behaviour(AgentEvent::RequestResponse(RequestResponseEvent::Message {
            peer,
            message,
        })) => match message {
            RequestResponseMessage::Request {
                request_id,
                request,
                channel,
            } => {
                let parsed_request =
                    AgentMessage::from_binary(&request).expect("Failed to decode request");
                match parsed_request {
                    AgentMessage::GreeRequest(req) => {
                        info!(
                                "RequestResponseEvent::Message::Request -> PeerID: {peer} | RequestID: \
                                 {request_id} | RequestMessage: {0:?}",
                                req.message
                            );
                    }
                    AgentMessage::AnotherMessage(msg) => {
                        info!(
                                "RequestResponseEvent::Message::Request -> PeerID: {peer} | RequestID: \
                                 {request_id} | AnotherMessage: {0:?}",
                                msg.info
                            );
                    }
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
                } else {
                    info!("Sending a message was success")
                }
            }
            RequestResponseMessage::Response {
                request_id,
                response,
            } => {
                let parsed_response =
                    AgentMessage::from_binary(&response).expect("Failed to decode response");
                match parsed_response {
                    AgentMessage::GreetResponse(res) => {
                        info!(
                                "RequestResponseEvent::Message::Response -> PeerID: {peer} | RequestID: \
                                 {request_id} | Response: {0:?}",
                                res.message
                            )
                    }
                    _ => {
                        info!("Received unknown response type.");
                    }
                }
            }
        },
        _ => {
            info!("swarm_event is {:?}", swarm_event);
        }
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
            info!("removing peer id {addr} so this address can be dialed by rust-libp2p");
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
