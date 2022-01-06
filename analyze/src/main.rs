use analyze::dataflow::construct_dataflow;
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::ToSocketAddrs;
use std::sync::{Arc, Mutex};
use timely::dataflow::operators::capture::Event;
use tokio::sync::RwLock;
use warp::Filter;
use warp::ws::Message;
use analyze::web;

fn main() {
    let (receiver, _handle) = construct_dataflow();
    let clients: web::Clients = Arc::new(RwLock::new(HashMap::new()));
    let ws_route = warp::path("ws")
        .and(warp::ws())
        // .and(warp::path::param())
        .and(with_clients(clients.clone()))
        .and_then(web::ws_handler);

    let routes = ws_route
        .with(warp::cors().allow_any_origin());

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();

    let (tsender, mut treceiver) = tokio::sync::mpsc::unbounded_channel();

    let receiver = Arc::new(Mutex::new(Some(receiver)));
    std::thread::spawn(move || {
        let receiver = receiver.lock().unwrap().take().unwrap();
        loop {
            match receiver.recv() {
                Ok(event) => tsender.send(event).unwrap(),
                Err(_) => break,
            }
        }
    });

    runtime.spawn(async move {
        loop {
            match treceiver.recv().await {
                Some(Event::Progress(progress)) => println!("Progress: {:?}", progress),
                Some(Event::Messages(time, data)) => {
                    println!("[{}] {:?}", time, data);
                    let result = serde_json::to_string(&data).unwrap();
                    clients.read().await.iter().for_each(|(_, client)| {
                        if let Some(sender) = &client.sender {
                            let _ = sender.send(Ok(Message::text(result.clone())));
                        }
                    })
                },
                None => break,
            }
        }
    });

    runtime.block_on(warp::serve(routes).run("localhost:8088".to_socket_addrs().unwrap().next().unwrap()));
}

fn with_clients(clients: web::Clients) -> impl Filter<Extract = (web::Clients,), Error = Infallible> + Clone {
    warp::any().map(move || clients.clone())
}
