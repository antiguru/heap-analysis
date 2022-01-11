use analyze::dataflow::construct_dataflow;
use analyze::web;
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::ToSocketAddrs;
use std::sync::{Arc, Mutex};
use timely::dataflow::operators::capture::Event;
use tokio::sync::RwLock;
use warp::http::Uri;
use warp::ws::Message;
use warp::Filter;

fn main() {
    pretty_env_logger::init();

    let (receiver, _handle) = construct_dataflow();
    let clients: web::Clients = Arc::new(RwLock::new(HashMap::new()));

    let log = warp::log("analyze::web");

    let ws_route = warp::path("ws")
        .and(warp::ws())
        // .and(warp::path::param())
        .and(with_clients(clients.clone()))
        .and_then(web::ws_handler);

    let static_route = warp::path("static").and(warp::fs::dir("analyze/static"));

    let index_redirect =
        warp::path::end().map(|| warp::redirect(Uri::from_static("/static/index.html")));

    let routes = ws_route
        .or(static_route)
        .or(index_redirect)
        .with(warp::cors().allow_any_origin())
        .with(log);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let (tsender, mut treceiver) = tokio::sync::mpsc::unbounded_channel();

    let receiver = Arc::new(Mutex::new(Some(receiver)));
    std::thread::spawn(move || {
        let receiver = receiver.lock().unwrap().take().unwrap();
        while let Ok(event) = receiver.recv() {
            tsender.send(event).unwrap()
        }
    });

    runtime.spawn(async move {
        loop {
            match treceiver.recv().await {
                Some(Event::Progress(_progress)) => {
                    // println!("Progress: {:?}", _progress)
                }
                Some(Event::Messages(_time, data)) => {
                    // println!("[{}] {:?}", _time, data);
                    let result = serde_json::to_string(&data).unwrap();
                    clients.read().await.iter().for_each(|(_, client)| {
                        if let Some(sender) = &client.sender {
                            let _ = sender.send(Ok(Message::text(result.clone())));
                        }
                    })
                }
                None => break,
            }
        }
    });

    runtime.block_on(
        warp::serve(routes).run("localhost:8088".to_socket_addrs().unwrap().next().unwrap()),
    );
}

fn with_clients(
    clients: web::Clients,
) -> impl Filter<Extract = (web::Clients,), Error = Infallible> + Clone {
    warp::any().map(move || clients.clone())
}
