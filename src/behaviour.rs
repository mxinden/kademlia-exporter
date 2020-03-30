use libp2p::{
    identify::{Identify, IdentifyEvent},
    kad::{record::store::MemoryStore, Kademlia, KademliaEvent},
    mdns::{Mdns, MdnsEvent},
    ping::{Ping, PingEvent},
    swarm::NetworkBehaviourEventProcess,
    NetworkBehaviour,
};
use prometheus::{CounterVec, Gauge};

#[derive(NetworkBehaviour)]
pub(crate) struct MyBehaviour {
    pub(crate) kademlia: Kademlia<MemoryStore>,
    pub(crate) mdns: Mdns,
    pub(crate) ping: Ping,
    pub(crate) identify: Identify,

    #[behaviour(ignore)]
    pub(crate) event_counter: CounterVec,
    #[behaviour(ignore)]
    pub(crate) kad_kbuckets_size: Gauge,
}

impl NetworkBehaviourEventProcess<MdnsEvent> for MyBehaviour {
    // Called when `mdns` produces an event.
    fn inject_event(&mut self, event: MdnsEvent) {
        match event {
            MdnsEvent::Discovered(list) => {
                self.event_counter
                    .with_label_values(&["mdns", "discovered"])
                    .inc();
                for (peer_id, multiaddr) in list {
                    self.kademlia.add_address(&peer_id, multiaddr);
                }
            }
            MdnsEvent::Expired(_) => {
                self.event_counter
                    .with_label_values(&["mdns", "expired"])
                    .inc();
            }
        }
    }
}

impl NetworkBehaviourEventProcess<PingEvent> for MyBehaviour {
    fn inject_event(&mut self, _event: PingEvent) {
        self.event_counter
            .with_label_values(&["ping", "ping_event"])
            .inc();
    }
}

impl NetworkBehaviourEventProcess<IdentifyEvent> for MyBehaviour {
    fn inject_event(&mut self, event: IdentifyEvent) {
        match event {
            IdentifyEvent::Error { .. } => {
                self.event_counter
                    .with_label_values(&["identify", "error"])
                    .inc();
            }
            IdentifyEvent::Sent { .. } => {
                self.event_counter
                    .with_label_values(&["identify", "sent"])
                    .inc();
            }
            IdentifyEvent::Received { .. } => {
                self.event_counter
                    .with_label_values(&["identify", "received"])
                    .inc();
            }
        }
    }
}

impl NetworkBehaviourEventProcess<KademliaEvent> for MyBehaviour {
    fn inject_event(&mut self, message: KademliaEvent) {
        match message {
            KademliaEvent::BootstrapResult(_) => {
                self.event_counter
                    .with_label_values(&["kad", "bootstrap"])
                    .inc();
            }
            KademliaEvent::GetClosestPeersResult(_) => {
                self.event_counter
                    .with_label_values(&["kad", "get_closest_peers"])
                    .inc();
            }
            KademliaEvent::GetProvidersResult(_) => {
                self.event_counter
                    .with_label_values(&["kad", "get_providers"])
                    .inc();
            }
            KademliaEvent::StartProvidingResult(_) => {
                self.event_counter
                    .with_label_values(&["kad", "start_providing"])
                    .inc();
            }
            KademliaEvent::RepublishProviderResult(_) => {
                self.event_counter
                    .with_label_values(&["kad", "republish_provider"])
                    .inc();
            }
            KademliaEvent::GetRecordResult(_) => {
                self.event_counter
                    .with_label_values(&["kad", "get_record"])
                    .inc();
            }
            KademliaEvent::PutRecordResult(_) => {
                self.event_counter
                    .with_label_values(&["kad", "put_record"])
                    .inc();
            }
            KademliaEvent::RepublishRecordResult(_) => {
                self.event_counter
                    .with_label_values(&["kad", "republish_record"])
                    .inc();
            }
            KademliaEvent::Discovered { .. } => {
                self.event_counter
                    .with_label_values(&["kad", "discovered"])
                    .inc();
            }
            KademliaEvent::RoutingUpdated { old_peer, .. } => {
                // Check if it is a new node, or just an update to a node.
                if old_peer.is_none() {
                    self.kad_kbuckets_size.inc();
                }
                self.event_counter
                    .with_label_values(&["kad", "routing_updated"])
                    .inc();
            }
            KademliaEvent::UnroutablePeer { .. } => {
                self.event_counter
                    .with_label_values(&["kad", "unroutable_peer"])
                    .inc();
            }
        }
    }
}
