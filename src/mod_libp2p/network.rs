use crate::mod_libp2p::behavior::{AgentBehavior, AgentEvent};
use futures_util::StreamExt;
use libp2p::{
    identify::Event as IdentifyEvent, kad::Event as KademliaEvent, ping::Event as PingEvent,
    swarm::SwarmEvent, Swarm,
};

pub(crate) struct EventLoop {
    swarm: Swarm<AgentBehavior>,
}

impl EventLoop {
    pub fn new(swarm: Swarm<AgentBehavior>) -> Self {
        Self { swarm }
    }

    pub(crate) async fn run(&mut self) {
        loop {
            tokio::select! {
                event = self.swarm.select_next_some() => self.handle_event(event).await,
            }
        }
    }

    pub async fn handle_event(&mut self, event: SwarmEvent<AgentEvent>) {
        match event {
            SwarmEvent::ConnectionEstablished { .. } => {}
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                self.swarm.behaviour_mut().kad.remove_peer(&peer_id);
            }
            SwarmEvent::Behaviour(AgentEvent::Identify(sub_event)) => {
                self.handle_identify_event(sub_event).await
            }
            SwarmEvent::Behaviour(AgentEvent::Kad(sub_event)) => {
                self.handle_kad_event(sub_event).await
            }
            SwarmEvent::Behaviour(AgentEvent::Ping(sub_event)) => {
                self.handle_ping_event(sub_event).await
            }
            _ => {}
        }
    }

    async fn handle_identify_event(&mut self, event: IdentifyEvent) {
        match event {
            IdentifyEvent::Received { peer_id, info, .. } => {
                for addr in info.clone().listen_addrs {
                    self.swarm.behaviour_mut().kad.add_address(&peer_id, addr);
                }
            }
            _ => {}
        }
    }

    async fn handle_kad_event(&mut self, _event: KademliaEvent) {}

    async fn handle_ping_event(&mut self, _event: PingEvent) {}
}
