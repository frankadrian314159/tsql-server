//! Sending Raft messages to the other replicas: one Arrow Flight action per message, fire and forget (Raft tolerates loss).
use crate::replica::Envelope;
use arrow_flight::client::FlightClient;
use arrow_flight::Action;
use futures::TryStreamExt;
use std::collections::HashMap;
use tokio::sync::mpsc::UnboundedReceiver;
use tonic::transport::{Channel, Endpoint};
use tsql_raft::{Msg, NodeId};
use tsql_shared_ces::wire::ACTION_RAFT;

pub fn spawn_sender(me: NodeId, peers: Vec<(NodeId, String)>, mut rx: UnboundedReceiver<(NodeId, Msg)>) {
    tokio::spawn(async move {
        let mut channels: HashMap<NodeId, Channel> = HashMap::new();
        for (id, addr) in &peers {
            if let Ok(ep) = Endpoint::from_shared(format!("http://{}", addr)) {
                channels.insert(*id, ep.tcp_nodelay(true).connect_timeout(std::time::Duration::from_millis(300)).connect_lazy());
            }
        }
        while let Some((to, msg)) = rx.recv().await {
            let Some(ch) = channels.get(&to).cloned() else { continue };
            let Ok(body) = postcard::to_allocvec(&Envelope { from: me, msg }) else { continue };
            tokio::spawn(async move {
                let mut c = FlightClient::new(ch);
                let send = async {
                    if let Ok(mut s) = c.do_action(Action::new(ACTION_RAFT, body)).await {
                        let _ = s.try_next().await;
                    }
                };
                let _ = tokio::time::timeout(std::time::Duration::from_millis(500), send).await;
            });
        }
    });
}
