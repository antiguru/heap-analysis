use crate::OutputData;
use std::collections::HashMap;
use std::sync::Arc;
use track_types::Timestamp;
use warp::{Rejection, Reply};

use futures::{FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::UnboundedReceiverStream;
use uuid::Uuid;
use warp::http::StatusCode;
use warp::reply::json;
use warp::ws::{Message, WebSocket};

pub type Clients = Arc<RwLock<HashMap<String, Client>>>;
type WebResult<T> = std::result::Result<T, Rejection>;

#[derive(Clone, Debug)]
pub struct Client {
    pub sender: Option<
        tokio::sync::mpsc::UnboundedSender<std::result::Result<warp::ws::Message, warp::Error>>,
    >,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RegisterRequest {}

#[derive(Debug, Deserialize, Serialize)]
pub struct RegisterResponse {
    url: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Event {
    timestamp: Timestamp,
    data: OutputData,
}

pub async fn publish_handler(event: Event, clients: Clients) -> WebResult<impl Reply> {
    let event = serde_json::to_string(&event).unwrap();
    clients.read().await.iter().for_each(|(_, client)| {
        if let Some(sender) = &client.sender {
            let _ = sender.send(Ok(Message::text(event.clone())));
        }
    });

    Ok(StatusCode::OK)
}

pub async fn register_handler(_body: RegisterRequest, clients: Clients) -> WebResult<impl Reply> {
    let uuid = Uuid::new_v4().to_string();

    register_client(uuid.clone(), clients).await;
    Ok(json(&RegisterResponse {
        url: format!("ws://localhost:8088/ws/{}", uuid),
    }))
}

async fn register_client(uuid: String, clients: Clients) {
    clients.write().await.insert(uuid, Client { sender: None });
}

pub async fn unregister_handler(id: String, clients: Clients) -> WebResult<impl Reply> {
    clients.write().await.remove(&id);
    Ok(StatusCode::OK)
}

pub async fn ws_handler(ws: warp::ws::Ws, clients: Clients) -> WebResult<impl Reply> {
    Ok(ws.on_upgrade(move |socket| client_connection(socket, clients)))
}

pub async fn client_connection(ws: WebSocket, clients: Clients) {
    let id = Uuid::new_v4().to_string();
    let (client_ws_sender, mut client_ws_rcv) = ws.split();
    let (client_sender, client_rcv) = mpsc::unbounded_channel();
    let client_rcv = UnboundedReceiverStream::new(client_rcv);
    tokio::task::spawn(client_rcv.forward(client_ws_sender).map(|result| {
        if let Err(e) = result {
            eprintln!("error sending websocket msg: {}", e);
        }
    }));

    let client = Client {
        sender: Some(client_sender),
    };
    clients.write().await.insert(id.clone(), client);

    println!("{} connected", id);

    while let Some(result) = client_ws_rcv.next().await {
        let msg = match result {
            Ok(msg) => msg,
            Err(e) => {
                eprintln!("error receiving ws message for id: {}): {}", id.clone(), e);
                break;
            }
        };
        client_msg(&id, msg).await;
    }

    clients.write().await.remove(&id);
    println!("{} disconnected", id);
}

async fn client_msg(id: &str, msg: Message) {
    println!("received message from {}: {:?}", id, msg);
}
