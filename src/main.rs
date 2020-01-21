use get_if_addrs::{IfAddr, Interface};
use rand::Rng;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use tokio::time::Duration;
use websocket::client::sync::Client;
use websocket::websocket_base::stream::sync::TcpStream;
use websocket::ws::dataframe::DataFrame;
use websocket::Message;
use log::{info, debug};

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
        std::thread::spawn(move || {
            let room = room_clone;
            let rxx = room.lock().unwrap().receiver.take().unwrap();
            let mut clients = Vec::<RoomClient>::new();
            let (tx, mut rx) = channel::<Message>();
            loop {
                //todo ping from channel
                match room.lock().unwrap().front.as_mut().unwrap()
                    .send_message(&Message::ping("ping".to_owned().into_bytes())) {
                    Ok(res) => (),
                    Err(e) => {
                        eprintln!("client room renderer {:?}", e);
                        break
                    }
                }
                if let Ok(new_client) = rxx.try_recv() {
                    clients.push(new_client);
                }
                clients.drain(..).for_each(|mut client| {
                    let tx = tx.clone();
                    std::thread::spawn(move || {
                        loop {
//                            match client.ws_client
//                                .send_message(&Message::ping("ping".to_owned().into_bytes())) {
//                                Ok(res) => (),
//                                Err(e) => {
//                                    eprintln!("client {:?}", e);
//                                    break
//                                }
//                            }
                            let recv = client.ws_client.recv_message();
                            match recv {
                                Ok(msg) if msg.is_data() => {
                                    info!("Receive msg from client: {:?}, body: {:?}", client.ip, msg);
                                    let result = tx.send(Message::from(msg));
                                    if result.is_err() { break; }
                                }
                                _ => break,
                            };
                        }
                    });
                });
                if let Ok(msg) = rx.try_recv() {
                    let result = room.lock()
                        .unwrap()
                        .front
                        .as_mut()
                        .unwrap()
                        .send_message(&msg);
                    if result.is_err() {
                        room.lock().unwrap().front.take();
                        eprintln!("{:?}", result);
                        break;
                    }
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


    let mut bc_aip = ip
        .unwrap()
        .iter()
        .map(|e| match e.addr {
            IfAddr::V4(ref e) => e.broadcast,
            _ => None,
        })
        .filter(Option::is_some)
        .collect::<Vec<_>>();
    let addr = bc_aip.pop().unwrap().unwrap();
    let mut socket: tokio::net::UdpSocket =
        tokio::net::UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0)))
            .await
            .unwrap();
    socket.set_broadcast(true);
    socket
        .connect(SocketAddr::from((addr.octets(), port_broadcast)))
        .await;
    let bytes = port.to_be_bytes();
    debug!(
        "send {:?}:{:?}, broadcast port: {}",
        addr.octets(),
        port,
        port_broadcast
    );
    socket.send(&bytes).await.unwrap();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::builder().format_timestamp_millis().init();
    let mut rng = rand::prelude::thread_rng();
    let mut port = 32111; //rng.gen_range(1i32, 65535i32);
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
    info!("Connected on: {}", init_server_port(port));
    let server_queue = raw_clients.clone();
    let server_rooms = rooms.clone();

    let connections = tokio::task::spawn(async move {
        let rng = &mut rand::prelude::thread_rng();
        loop {
            println!("connect wait");
            let client_upgrade = match server.accept() {
                Ok(client) => client,
                Err(err) => {
                    eprintln!("{}", err.error);
                    continue;
                }
            };
            println!("connect accept {:?}", client_upgrade.origin());
            if client_upgrade.origin().is_some() {
                let mut room_id = rng.gen_range(100usize, 300usize);
                let mut room_guard = server_rooms.lock().unwrap();
                let mut room = room_guard.iter().find(|e| e.lock().unwrap().id == room_id);
                while room.is_some() {
                    room_id = rng.gen_range(100usize, 300usize);
                    room = room_guard.iter().find(|e| e.lock().unwrap().id == room_id);
                }
                info!("Front connected {:?} {:?} with room {}", client_upgrade.origin(), client_upgrade.stream.peer_addr(), room_id);
                let client = client_upgrade.accept();
                if client.is_err() {
                    info!("Front disconnected");
                    continue;
                }
                let mut browser = client.unwrap();
                browser.send_message(&Message::text(&format!("0.0 {}", room_id)));
                room_guard.push(Room::new(room_id, Some(browser)));
                continue;
            }
            let client = client_upgrade.accept();
            if client.is_err() {
                info!("Client disconnected");
                continue;
            }
            let connected_client = client.unwrap();
            let client_ip = get_client_ip(&connected_client.peer_addr());
            info!("Connected new client: {:?}", client_ip);
            server_queue.lock().unwrap().push(RoomClient {
                ip: client_ip,
                ws_client: connected_client,
            });
        }
    });
    let clients = tokio::task::spawn(async move {
        loop {
            if let Ok(mut clients) = raw_clients.try_lock() {
                if rooms.lock().unwrap().is_empty() {
                    continue;
                }
                clients.drain(..).for_each(|mut raw_client| {
                    match raw_client.ws_client.recv_message() {
                        Ok(msg) => if msg.is_data() {
                            let id = String::from_utf8(msg.take_payload())
                                .map(|cmd| {
                                    let room_id = cmd.split("room").collect::<String>();
                                    usize::from_str(&room_id).unwrap_or(0)
                                })
                                .unwrap_or(0);
                            info!("Client {:?} want to connect in the room with id {}", raw_client.ip, id);
                            let room_id = rooms.lock().unwrap().iter_mut().position(|r| r.lock().unwrap().id == id);
                            if room_id.is_some() {
                                let room = rooms.lock().unwrap().get(room_id.unwrap()).unwrap().clone();
                                if room.lock().unwrap().front.is_none() {
                                    rooms.lock().unwrap().remove(room_id.unwrap());
                                }
                                info!("Client {:?} connected to the room {}", raw_client.ip, id);
                                room.lock()
                                    .unwrap()
                                    .sender
                                    .as_ref()
                                    .unwrap()
                                    .send(raw_client);
                            } else {
                                info!("Client rejected");
                            }
                        }
                        _ => eprintln!("Broken pipe"),
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
        _ => [0; 4],
    }
}

fn init_server_port(port: i32) -> String {
    format!("{}:{}", LOCAL_HOST, port)
}
