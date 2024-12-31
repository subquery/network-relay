use crate::mod_libp2p::{
    behavior::{AgentBehavior, AgentEvent},
    message::{AgentMessage, GreetRequest},
};
use futures_util::StreamExt;
use libp2p::{
    core::ConnectedPoint,
    gossipsub::Event as GossipsubEvent,
    identify::Event as IdentifyEvent,
    kad::Event as KademliaEvent,
    mdns::Event as MdnsEvent,
    ping::Event as PingEvent,
    request_response::{Event as RequestResponseEvent, Message, OutboundRequestId},
    swarm::ConnectionId,
    swarm::SwarmEvent,
    PeerId, Swarm,
};
use std::collections::HashMap;
use tokio::time::{self, Duration};
use tracing::warn;

pub(crate) struct EventLoop {
    swarm: Swarm<AgentBehavior>,
}

impl EventLoop {
    pub fn new(swarm: Swarm<AgentBehavior>) -> Self {
        Self { swarm }
    }

    pub(crate) async fn run(&mut self) {
        let mut interval = time::interval(Duration::from_secs(10));
        loop {
            tokio::select! {
                event = self.swarm.select_next_some() => self.handle_event(event).await,
                _ = interval.tick() => {
                    let key = self.swarm.local_peer_id().to_base58().into_bytes().into();
                    if let Err(err) = self.swarm.behaviour_mut().kad.start_providing(key) {
                        warn!("err is {:?}", err);
                    }
                }
            }
        }
    }

    pub(crate) async fn start_provider(&mut self) {
        let key = self.swarm.local_peer_id().to_base58().into_bytes().into();
        if let Err(err) = self.swarm.behaviour_mut().kad.start_providing(key) {
            warn!("err is {:?}", err);
        }
    }

    pub async fn handle_event(&mut self, event: SwarmEvent<AgentEvent>) {
        warn!("event is {:?}", event);
        match event {
            SwarmEvent::ConnectionEstablished {
                peer_id, endpoint, ..
            } => {
                match endpoint {
                    ConnectedPoint::Dialer { address, .. } => {
                        warn!("file: {}, line: {}", file!(), line!());
                        _ = self
                            .swarm
                            .behaviour_mut()
                            .kad
                            .add_address(&peer_id, address)
                    }
                    _ => {
                        warn!(
                            "file: {}, line: {}, endpoint: {:?}",
                            file!(),
                            line!(),
                            endpoint
                        );
                    }
                };
            }
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                self.swarm.behaviour_mut().kad.remove_peer(&peer_id);
            }
            SwarmEvent::Behaviour(AgentEvent::Identify(sub_event)) => {
                self.handle_identify_event(sub_event).await
            }
            SwarmEvent::Behaviour(AgentEvent::Kad(sub_event)) => {
                self.handle_kad_event(sub_event).await
            }
            SwarmEvent::Behaviour(AgentEvent::RequestResponse(sub_event)) => {
                self.handle_request_response_event(sub_event).await
            }
            SwarmEvent::Behaviour(AgentEvent::Gossipsub(sub_event)) => {
                self.handle_gossipsub_event(sub_event).await
            }
            SwarmEvent::Behaviour(AgentEvent::Ping(sub_event)) => {
                self.handle_ping_event(sub_event).await
            }
            SwarmEvent::Behaviour(AgentEvent::Mdns(sub_event)) => {
                self.handle_mdns_event(sub_event).await
            }
            _ => warn!("not handled event is {:?}", event),
        }
    }

    async fn handle_identify_event(&mut self, event: IdentifyEvent) {
        match event {
            IdentifyEvent::Received { peer_id, info, .. } => {
                warn!(
                    "peer_id.to_base58() : {:?}, METRICS_PEER_ID",
                    peer_id.to_base58(),
                );
                for addr in info.clone().listen_addrs {
                    warn!(" metrics peer found, addr is {:?}", addr);
                    // _ = self.swarm.dial(addr);
                    self.swarm.behaviour_mut().kad.add_address(&peer_id, addr);
                }
            }
            _ => {}
        }
    }

    async fn handle_kad_event(&mut self, event: KademliaEvent) {
        warn!("kad event is {:?}", event);
    }

    async fn handle_request_response_event(
        &mut self,
        event: RequestResponseEvent<AgentMessage, AgentMessage>,
    ) {
    }

    async fn handle_gossipsub_event(&mut self, event: GossipsubEvent) {}

    async fn handle_ping_event(&mut self, event: PingEvent) {}

    async fn handle_mdns_event(&mut self, event: MdnsEvent) {}
}
