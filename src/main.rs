use websocket::Message;
use websocket::ws::dataframe::DataFrame;
use std::sync::{Mutex, Arc};
use websocket::server::{WsServer, NoTlsAcceptor};
use websocket::r#async::TcpListener;
use rand::Rng;
use websocket::client::sync::Client;
use std::net::{IpAddr, SocketAddr, SocketAddrV4, UdpSocket};
use websocket::websocket_base::stream::sync::TcpStream;
use futures::{StreamExt, TryStreamExt};
use std::str::FromStr;
use get_if_addrs::{Interface, IfAddr};
use tokio::time::{Duration, Instant};
use futures::task::SpawnExt;
use std::sync::mpsc::{channel, Sender, Receiver};

const LOCAL_HOST: &str = "0.0.0.0";

struct Room {
    id: usize,
    front: Option<Client<TcpStream>>,
    sender: Option<Sender<RoomClient>>,
    receiver: Option<Receiver<RoomClient>>,
}

impl Room {
    fn new(id: usize, front: Option<Client<TcpStream>>) -> Arc<Mutex<Room>> {
        let (tx, rx) = channel::<RoomClient>();
        let room = Arc::new(Mutex::new(Room {
            id,
            front,
            sender: Some(tx),
            receiver: Some(rx),
        }));
        let room_clone = room.clone();
        tokio::task::spawn(async move {
            let room = room_clone;
            let rx = room.lock().unwrap().receiver.take().unwrap();
            let mut clients = Vec::<RoomClient>::new();
            loop {
                while let Ok(new_client) = rx.try_recv() {
                    clients.push(new_client);
                }
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
                clients.drain(..).for_each(|mut client| {
                    let tx = tx.clone();
                    tokio::task::spawn(async move {
                        loop {
                            let recv = client.ws_client.recv_message();
                            match recv {
                                Ok(msg) if msg.is_data() => {
                                    tx.send(Message::from(msg)).unwrap();
                                }
                                _ => break
                            };
                        }
                    });
                });
                while let Some(msg) = rx.recv().await {
                    room.lock().unwrap().front.as_mut().unwrap().send_message(&msg).unwrap();
                }
            }
        });
        room
    }
}

struct RoomClient {
    ip: [u8; 4],
    ws_client: Client<TcpStream>,
}

async fn send_my_ip_to_all(port: i32, port_broadcast: u16) {
    let ip: std::io::Result<Vec<Interface>> = get_if_addrs::get_if_addrs();
    let mut bc_aip = ip.unwrap()
        .iter()
        .map(|e| {
            match e.addr {
                IfAddr::V4(ref e) => e.broadcast,
                _ => None
            }
        }).filter(Option::is_some).collect::<Vec<_>>();
    let addr = bc_aip.pop().unwrap().unwrap();
    let mut socket: tokio::net::UdpSocket = tokio::net::UdpSocket::bind(
        SocketAddr::from(([0, 0, 0, 0], 0))
    ).await.unwrap();
    socket.set_broadcast(true);
    socket.connect(SocketAddr::from((addr.octets(), port_broadcast))).await;
    let bytes = port.to_be_bytes();
    println!("send {:?}:{:?}, broadcast port: {}", addr.octets(), port, port_broadcast);
    socket.send(&bytes).await.unwrap();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut rng = rand::prelude::thread_rng();
    let mut port = 26199; //rng.gen_range(1i32, 65535i32);
    let rooms = Arc::new(Mutex::new(Vec::<Arc<Mutex<Room>>>::new()));
    let mut server = loop {
        let server = websocket::server::sync::Server::bind(init_server_port(port));
        if server.is_err() {
            port = rng.gen_range(1i32, 100000i32);
            continue;
        } else {
            break server.unwrap();
        }
    };
    let port_broadcast = u16::from_str(&std::env::args().skip(1).next().unwrap()).unwrap();
    tokio::task::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(3));
        loop {
            timer.tick().await;
            send_my_ip_to_all(port, port_broadcast).await;
        }
    });
    let raw_clients: Arc<Mutex<Vec<RoomClient>>> = Arc::new(Mutex::new(Vec::<RoomClient>::new()));
    println!("Connected on: {}", init_server_port(port));

    let server_queue = raw_clients.clone();
    let server_rooms = rooms.clone();

    let connections = tokio::task::spawn(async move {
        let rng = &mut rand::prelude::thread_rng();
        loop {
            let client_upgrade = match server.accept() {
                Ok(client) => {
                    client
                }
                Err(err) => {
                    eprint!("{}", err.error);
                    continue;
                }
            };
            if client_upgrade.origin().is_some() {
                let (tx, rx) = channel::<RoomClient>();
                let mut room_id = rng.gen_range(100usize, 300usize);
                let mut room_guard = server_rooms.lock().unwrap();
                let mut room = room_guard.iter().find(|e| e.lock().unwrap().id == room_id);
                while room.is_some() {
                    room_id = rng.gen_range(100usize, 300usize);
                    room = room_guard.iter().find(|e| e.lock().unwrap().id == room_id);
                }
                let mut client = client_upgrade.accept();
                if client.is_err() {
                    continue;
                }
                room_guard.push(Room::new(room_id, Some(client.unwrap())));
                continue;
            }
            let mut client = client_upgrade.accept();
            if client.is_err() {
                continue;
            }
            let connected_client = client.unwrap();
            server_queue.lock().unwrap().push(RoomClient {
                ip: get_client_ip(&connected_client.peer_addr()),
                ws_client: connected_client,
            })
        }
    });
    let clients = tokio::task::spawn(async move {
        loop {
            if let (Ok(mut clients), Ok(mut rooms))
            = (raw_clients.try_lock(), rooms.lock()) {
                if rooms.is_empty() {
                    continue;
                }
                clients.drain(..).for_each(|mut raw_client| {
                    match raw_client.ws_client.recv_message() {
                        Ok(msg) if msg.is_data() => {
                            let id = String::from_utf8(msg.take_payload())
                                .map(|cmd| {
                                    let room_id = cmd.split("room").next().unwrap_or("0");
                                    usize::from_str(room_id).unwrap_or(0)
                                }).unwrap_or(0);
                            let room = rooms.iter_mut()
                                .find(|r| r.lock().unwrap().id == id);
                            if room.is_some() {
                                room.unwrap().lock().unwrap().sender.as_ref().unwrap().send(raw_client);
                            }
                        }
                        _ => eprintln!("Broken pipe")
                    }
                });
            }
        }
    });
    futures::future::join(connections, clients).await;
    Ok(())
}

fn get_client_ip(socket_addr: &std::io::Result<SocketAddr>) -> [u8; 4] {
    match socket_addr {
        Ok(SocketAddr::V4(socket_addr)) => socket_addr.ip().octets(),
        _ => [0; 4]
    }
}

fn init_server_port(port: i32) -> String {
    format!("{}:{}", LOCAL_HOST, port)
}
