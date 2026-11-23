//! Sending Raft messages to the other replicas: one Arrow Flight action per message, fire and forget (Raft tolerates loss).
use crate::replica::Envelope;
use arrow_flight::client::FlightClient;
use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::Action;
use futures::TryStreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedReceiver;
use tonic::transport::{Channel, Endpoint};
use tsql_raft::{Msg, NodeId};
use tsql_shared_ces::wire::ACTION_RAFT;

/// Where each replica can be reached. The replica keeps it up to date as the membership changes; a message to a replica it
/// does not list is dropped.
pub type AddressBook = Arc<Mutex<HashMap<NodeId, String>>>;

pub fn spawn_sender(me: NodeId, book: AddressBook, mut rx: UnboundedReceiver<(NodeId, Msg)>) {
    tokio::spawn(async move {
        // a channel per replica, made again when its address changes
        let mut channels: HashMap<NodeId, (String, Channel)> = HashMap::new();
        while let Some((to, msg)) = rx.recv().await {
            let addr = book.lock().unwrap_or_else(|e| e.into_inner()).get(&to).cloned();
            let Some(addr) = addr else { continue };
            let ch = match channels.get(&to) {
                Some((a, ch)) if *a == addr => ch.clone(),
                _ => {
                    let Ok(ep) = Endpoint::from_shared(format!("http://{}", addr)) else { continue };
                    let ch = ep.tcp_nodelay(true).connect_timeout(std::time::Duration::from_millis(300)).connect_lazy();
                    channels.insert(to, (addr, ch.clone()));
                    ch
                }
            };
            let Ok(body) = postcard::to_allocvec(&Envelope { from: me, msg }) else { continue };
            tokio::spawn(async move {
                let mut c = FlightClient::new_from_inner(
                    FlightServiceClient::new(ch)
                        .max_decoding_message_size(128 * 1024 * 1024)
                        .max_encoding_message_size(128 * 1024 * 1024),
                );
                let send = async {
                    if let Ok(mut s) = c.do_action(Action::new(ACTION_RAFT, body)).await {
                        let _ = s.try_next().await;
                    }
                };
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), send).await;
            });
        }
    });
}
