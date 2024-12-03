use mod_libp2p::{handle_swarm_event, start_swarm};
use std::io::Result;
use std::net::SocketAddr;
use std::path::PathBuf;
use subql_indexer_utils::{constants::BOOTSTRAP, p2p::ROOT_GROUP_ID};
use tdn::prelude::{
    channel_rpc_channel, start_with_config, Config, HandleResult, NetworkType, Peer,
    ReceiveMessage, RecvType, SendMessage, SendType,
};
use tokio::sync::mpsc::Sender;
use tracing::info;
mod mod_libp2p;

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    std::env::set_var("RUST_LOG", "info");

    tracing_subscriber::fmt()
        .with_ansi(false)
        .event_format(
            tracing_subscriber::fmt::format()
                .with_file(true)
                .with_line_number(true),
        )
        .init();

    let addr_str = std::env::args().nth(1).unwrap_or("0.0.0.0:7370".to_owned());
    let addr: SocketAddr = addr_str.parse().expect("invalid addr");

    // start new network
    let (out_send, mut out_recv, _inner_send, inner_recv) = channel_rpc_channel();
    tokio::spawn(async move {
        while let Some(msg) = out_recv.recv().await {
            println!("GOT NOT HANDLE RPC: {:?}", msg);
        }
    });
    println!("* P2P  listening: {}", addr);

    let mut config = Config::default();

    config.only_stable_data = false;
    config.db_path = Some(PathBuf::from("./.data/p2p"));
    config.rpc_http = None;
    config.p2p_peer = Peer::socket(addr);
    config.rpc_channel = Some((out_send, inner_recv));
    config.group_ids = vec![ROOT_GROUP_ID];

    let (peer_addr, send, mut out_recv) = start_with_config(config).await.unwrap();
    println!("* TDN PEER ID       : {:?}", peer_addr);

    bootstrap(&send).await;

    match start_swarm().await {
        Ok(swarm) => handle_swarm_event(swarm).await,
        Err(err) => info!("start libp2p swarm failed, the err is {:?}", err),
    }

    while let Some(message) = out_recv.recv().await {
        match message {
            ReceiveMessage::Group(msg) => {
                if let Ok(result) = handle_group(msg).await {
                    handle_result(result, &send).await;
                }
            }
            ReceiveMessage::NetworkLost => {
                // println!("No network connections, will re-connnect");
                bootstrap(&send).await;
            }
            _ => {
                println!("Nothing about this message");
            }
        }
    }
}

async fn handle_result(result: HandleResult, sender: &Sender<SendMessage>) {
    let HandleResult {
        owns: _,
        rpcs: _,
        mut groups,
        mut networks,
    } = result;

    loop {
        if groups.len() != 0 {
            let msg = groups.remove(0);
            sender
                .send(SendMessage::Group(msg))
                .await
                .expect("TDN channel closed");
        } else {
            break;
        }
    }

    // must last send, because it will has stop type.
    loop {
        if networks.len() != 0 {
            let msg = networks.remove(0);
            sender
                .send(SendMessage::Network(msg))
                .await
                .expect("TDN channel closed");
        } else {
            break;
        }
    }
}

async fn handle_group(msg: RecvType) -> Result<HandleResult> {
    let mut results = HandleResult::new();

    match msg {
        RecvType::Connect(peer, _bytes) => {
            println!("New peer {} join", peer.id.short_show());
            let msg = SendType::Result(0, peer, false, false, vec![]);
            results.groups.push(msg);
        }
        _ => {}
    }

    Ok(results)
}

async fn bootstrap(sender: &Sender<SendMessage>) {
    // let projects: Vec<String> = vec![ROOT_NAME.to_owned()];
    // let self_bytes = bincode::serialize(&JoinData(projects)).unwrap_or(vec![]);

    for seed in &BOOTSTRAP {
        if let Ok(addr) = seed.trim().parse() {
            let peer = Peer::socket(addr);
            sender
                .send(SendMessage::Network(NetworkType::Connect(peer)))
                .await
                .expect("TDN channel closed");
        }
    }
}
