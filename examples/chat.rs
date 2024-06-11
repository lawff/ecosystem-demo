use anyhow::Result;
use core::fmt;
use dashmap::DashMap;
use futures::{stream::SplitStream, SinkExt, StreamExt};
use std::{net::SocketAddr, sync::Arc};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_util::codec::{Framed, LinesCodec};
use tracing::{info, level_filters::LevelFilter, warn};
use tracing_subscriber::{fmt::Layer, layer::SubscriberExt, util::SubscriberInitExt, Layer as _};

const MAX_MESSAGE: usize = 128;

#[derive(Debug, Default)]
struct AppState {
    peers: DashMap<SocketAddr, mpsc::Sender<Arc<Message>>>,
}

#[derive(Debug)]
struct Peer {
    username: String,
    stream: SplitStream<Framed<TcpStream, LinesCodec>>,
}

#[derive(Debug)]
enum Message {
    UserJoined(String),
    UserLeft(String),
    Chat { name: String, message: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let layer = Layer::new().with_filter(LevelFilter::INFO);
    tracing_subscriber::registry().with(layer).init();

    let addr = "0.0.0.0:8080";
    let listener = TcpListener::bind(addr).await?;
    info!("Listening on: {}", addr);
    let state = Arc::new(AppState::default());

    loop {
        let (stream, raddr) = listener.accept().await?;
        info!("Accepted connection from: {}", raddr);
        let state_cloned = state.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_client(raddr, stream, state_cloned).await {
                warn!("Error handling client {}: {:?}", raddr, err);
            }
        });
    }

    #[allow(unreachable_code)]
    Ok(())
}

async fn handle_client(addr: SocketAddr, stream: TcpStream, state: Arc<AppState>) -> Result<()> {
    let mut lines = Framed::new(stream, LinesCodec::new());
    lines.send("Please enter your username:").await?;

    // TODO: 循环读取名字，直到不为空以及名字重复问题
    let username = match lines.next().await {
        Some(Ok(line)) => {
            if line.trim().is_empty() {
                lines.send("Empty username error!!!").await?;
                return Err(anyhow::anyhow!("Empty username"));
            } else {
                line
            }
        }
        _ => {
            warn!("Failed to read username from client: {}", addr);
            return Err(anyhow::anyhow!("Failed to read username"));
        }
    };

    let mut peer = state.add(addr, username, lines).await;

    let message = Arc::new(Message::UserJoined(peer.username.clone()));
    info!("{}", message);
    state.broadcast(addr, message).await;

    while let Some(line) = peer.stream.next().await {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                warn!("Error reading line from client {}: {:?}", addr, err);
                break;
            }
        };

        let message = Arc::new(Message::Chat {
            name: peer.username.clone(),
            message: line,
        });
        info!("{}", message);
        state.broadcast(addr, message).await;
    }

    state.peers.remove(&addr);
    let message = Arc::new(Message::UserLeft(peer.username.clone()));
    info!("{}", message);
    state.broadcast(addr, message).await;

    Ok(())
}

impl AppState {
    async fn broadcast(&self, addr: SocketAddr, message: Arc<Message>) {
        for peer in self.peers.iter() {
            if peer.key() == &addr {
                continue;
            }
            if let Err(err) = peer.value().send(message.clone()).await {
                warn!("Error sending message to {}: {:?}", addr, err);
            }
        }
    }

    async fn add(
        &self,
        addr: SocketAddr,
        username: String,
        stream: Framed<TcpStream, LinesCodec>,
    ) -> Peer {
        let (tx, mut rx) = mpsc::channel(MAX_MESSAGE);
        self.peers.insert(addr, tx);

        let (mut stream_sender, stream_receiver) = stream.split();

        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if let Err(err) = stream_sender.send(message.to_string()).await {
                    warn!("Error sending message to {}: {:?}", addr, err);
                    break;
                }
            }
        });

        Peer {
            username,
            stream: stream_receiver,
        }
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Message::UserJoined(username) => write!(f, "[{}] has joined the chat", username),
            Message::UserLeft(username) => write!(f, "[{}] has left the chat", username),
            Message::Chat { name, message } => write!(f, "[{}]: {}", name, message),
        }
    }
}
