use super::id::{broadcast_machine_id, MachineID, RawID};
use super::inbox::Inbox;
use super::messaging::{Message, Packet};
use super::type_registry::ShortTypeId;
use byteorder::{ByteOrder, LittleEndian, WriteBytesExt};
use compact::Compact;
#[cfg(feature = "server")]
use std::net::{TcpListener, TcpStream};
use std::time::Duration;
#[cfg(feature = "browser")]
use stdweb::traits::{IEventTarget, IMessageEvent};
#[cfg(feature = "browser")]
use stdweb::web::{SocketBinaryType, SocketReadyState, TypedArray, WebSocket};
#[cfg(feature = "server")]
use tungstenite::util::NonBlockingError;
#[cfg(feature = "server")]
use tungstenite::{
    accept as websocket_accept, client as websocket_client, Message as WebSocketMessage, WebSocket,
};
#[cfg(feature = "server")]
use url::Url;

/// Represents all networking environment and networking state
/// of an `ActorSystem`
pub struct Networking {
    /// The machine index of this machine within the network of peers
    pub machine_id: MachineID,
    batch_message_bytes: usize,
    /// The current network turn this machine is in. Used to keep track
    /// if this machine lags behind or runs fast compared to its peers
    pub n_turns: usize,
    acceptable_turn_distance: usize,
    turn_sleep_distance_ratio: usize,
    network: Vec<&'static str>,
    network_connections: Vec<Option<Connection>>,
    #[cfg(feature = "server")]
    listener: TcpListener,
}

impl Networking {
    /// Create network environment based on this machines id/index
    /// and all peer addresses (including this machine)
    pub fn new(
        machine_id: u8,
        network: Vec<&'static str>,
        batch_message_bytes: usize,
        acceptable_turn_distance: usize,
        turn_sleep_distance_ratio: usize,
    ) -> Networking {
        #[cfg(feature = "server")]
        let listener = {
            let listener = TcpListener::bind(network[machine_id as usize]).unwrap();
            listener.set_nonblocking(true).unwrap();
            listener
        };

        Networking {
            machine_id: MachineID(machine_id),
            batch_message_bytes,
            n_turns: 0,
            acceptable_turn_distance,
            turn_sleep_distance_ratio,
            network_connections: (0..network.len()).into_iter().map(|_| None).collect(),
            network,
            #[cfg(feature = "server")]
            listener,
        }
    }

    #[cfg(feature = "server")]
    /// Try to connect to peers in the network
    pub fn connect(&mut self) {
        // first wait for a larger machine_id to connect
        if self
            .network_connections
            .iter()
            .enumerate()
            .any(|(machine_id, connection)| {
                machine_id > self.machine_id.0 as usize && connection.is_none()
            }) {
            match self.listener.accept() {
                Ok((stream, addr)) => {
                    println!("Got connection from {}, shaking hands...", addr);
                    match websocket_accept(stream) {
                        Ok(mut websocket) => loop {
                            match websocket.read_message() {
                                Ok(WebSocketMessage::Binary(data)) => {
                                    let peer_machine_id = data[0];
                                    self.network_connections[peer_machine_id as usize] =
                                        Some(Connection::new(websocket, self.batch_message_bytes));
                                    println!("...machine ID {} connected!", peer_machine_id);
                                    break;
                                }
                                Ok(_) => {}
                                Err(e) => if let Some(real_err) = e.into_non_blocking() {
                                    println!("Error while expecting first message: {}", real_err);
                                    break;
                                },
                            }
                        },
                        Err(e) => println!("Error while accepting connection: {}", e),
                    }
                }
                Err(ref e) if e.kind() == ::std::io::ErrorKind::WouldBlock => {}
                Err(e) => println!("Error while accepting connection: {}", e),
            }
        }

        // then try to connect to all smaller machine_ids
        for (machine_id, address) in self.network.iter().enumerate() {
            if machine_id < self.machine_id.0 as usize {
                if self.network_connections[machine_id].is_none() {
                    let stream = TcpStream::connect(address).unwrap();
                    stream.set_read_timeout(None).unwrap();
                    stream.set_write_timeout(None).unwrap();
                    let mut websocket =
                        websocket_client(Url::parse(&format!("ws://{}", address)).unwrap(), stream)
                            .unwrap()
                            .0;
                    match websocket
                        .write_message(WebSocketMessage::binary(vec![self.machine_id.0]))
                        .and_then(|_| websocket.write_pending())
                    {
                        Ok(_) => {}
                        Err(e) => panic!("Error while sending first message: {}", e),
                    }
                    self.network_connections[machine_id] =
                        Some(Connection::new(websocket, self.batch_message_bytes));
                    println!("Connected to Machine ID {}", machine_id);
                }
            }
        }
    }

    #[cfg(feature = "browser")]
    /// Connect to all peers in the network
    pub fn connect(&mut self) {
        for (machine_id, address) in self.network.iter().enumerate() {
            if machine_id != self.machine_id.0 as usize {
                if self.network_connections[machine_id].is_none() {
                    let websocket = WebSocket::new(&format!("ws://{}", address)).unwrap();
                    let mut connection = Some(Connection::new(websocket, self.batch_message_bytes));
                    connection
                        .as_mut()
                        .unwrap()
                        .out_batches
                        .insert(0, vec![self.machine_id.0]);
                    self.network_connections[machine_id] = connection;
                }
            }
        }
    }

    /// Finish the current networking turn and wait for peers which lag behind
    /// based on their turn number. This is the main backpressure mechanism.
    pub fn finish_turn(&mut self) -> Option<Duration> {
        let mut should_sleep = None;

        for maybe_connection in &mut self.network_connections {
            if let Some(Connection { n_turns, .. }) = *maybe_connection {
                if n_turns + self.acceptable_turn_distance < self.n_turns {
                    should_sleep = Some(Duration::from_millis(
                        ((self.n_turns - self.acceptable_turn_distance - n_turns)
                            / self.turn_sleep_distance_ratio) as u64,
                    ));
                }
            }
        }

        self.n_turns += 1;

        for maybe_connection in self.network_connections.iter_mut() {
            if let Some(ref mut connection) = *maybe_connection {
                // write turn end, use 0 as "message type" to distinguish from actual packet
                {
                    let mut data = connection.enqueue_in_batch(
                        ::std::mem::size_of::<ShortTypeId>() + ::std::mem::size_of::<u32>(),
                    );
                    data.write_u16::<LittleEndian>(0).unwrap();
                    data.write_u32::<LittleEndian>(self.n_turns as u32).unwrap();
                }
                connection.n_turns_since_own_turn = 0;
            }
        }

        should_sleep
    }

    /// Send queued outbound messages and take incoming queued messages
    /// and forward them to their local target recipient(s)
    pub fn send_and_receive(&mut self, inboxes: &mut [Option<Inbox>]) {
        self.connect();

        for (machine_id, maybe_connection) in self.network_connections.iter_mut().enumerate() {
            let closed_reason = if let Some(ref mut connection) = *maybe_connection {
                match connection
                    .try_send_pending()
                    .and_then(|_| connection.try_receive(inboxes))
                {
                    Ok(()) => None,
                    Err(err) => Some(err),
                }
            } else {
                None
            };

            if let Some(closed_reason) = closed_reason {
                println!(
                    "Closed connection to Machine ID {} while receiving: {}",
                    machine_id, closed_reason
                );
                *maybe_connection = None
            }
        }

        #[cfg(feature = "browser")]
        {
            let max_n_turns = self
                .network_connections
                .iter()
                .map(|maybe_connection| {
                    if let Some(connection) = maybe_connection {
                        connection.n_turns
                    } else {
                        0
                    }
                })
                .max()
                .unwrap_or(self.n_turns);

            if max_n_turns > 1000 + self.n_turns {
                self.n_turns = max_n_turns;
            }
        }
    }

    /// Enqueue a new (potentially) outbound packet
    pub fn enqueue<M: Message>(&mut self, message_type_id: ShortTypeId, mut packet: Packet<M>) {
        if self.network.len() == 1 {
            return;
        }

        let packet_size = Compact::total_size_bytes(&packet);
        let total_size = ::std::mem::size_of::<ShortTypeId>() + packet_size;
        let machine_id = packet.recipient_id.machine;

        let recipients = if machine_id == broadcast_machine_id() {
            (0..self.network.len()).into_iter().collect()
        } else {
            vec![machine_id.0 as usize]
        };

        for machine_id in recipients {
            if let Some(connection) = self.network_connections[machine_id].as_mut() {
                let mut data = connection.enqueue_in_batch(total_size);
                data.write_u16::<LittleEndian>(message_type_id.into())
                    .unwrap();
                let packet_pos = data.len();
                data.resize(packet_pos + packet_size, 0);

                unsafe {
                    // store packet compactly in write queue
                    Compact::compact_behind(
                        &mut packet,
                        &mut data[packet_pos] as *mut u8 as *mut Packet<M>,
                    );
                }
            }
        }

        ::std::mem::forget(packet);
    }

    /// Return a debug message containing the current local view of
    /// network turn progress of all peers in the network
    pub fn debug_all_n_turns(&self) -> String {
        self.network_connections
            .iter()
            .enumerate()
            .map(|(i, maybe_connection)| {
                format!(
                    "{}: {}",
                    i,
                    if i == usize::from(self.machine_id.0) {
                        self.n_turns as isize
                    } else {
                        if let Some(connection) = maybe_connection.as_ref() {
                            connection.n_turns as isize
                        } else {
                            -1
                        }
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(",\n")
    }
}

#[cfg(feature = "server")]
pub struct Connection {
    n_turns: usize,
    n_turns_since_own_turn: usize,
    websocket: WebSocket<TcpStream>,
    out_batches: Vec<Vec<u8>>,
    batch_message_bytes: usize,
}

#[cfg(feature = "server")]
impl Connection {
    pub fn new(mut websocket: WebSocket<TcpStream>, batch_message_bytes: usize) -> Connection {
        {
            let tcp_socket = websocket.get_mut();
            tcp_socket.set_nonblocking(true).unwrap();
            tcp_socket.set_read_timeout(None).unwrap();
            tcp_socket.set_write_timeout(None).unwrap();
            tcp_socket.set_nodelay(true).unwrap();
        }
        Connection {
            n_turns: 0,
            n_turns_since_own_turn: 0,
            websocket,
            out_batches: vec![Vec::with_capacity(batch_message_bytes)],
            batch_message_bytes,
        }
    }

    pub fn enqueue_in_batch(&mut self, message_size: usize) -> &mut Vec<u8> {
        // let recipient_id =
        //     (&message[::std::mem::size_of::<ShortTypeId>()] as *const u8) as *const RawID;
        // println!(
        //     "Enqueueing message recipient: {:?}, data: {:?}",
        //     unsafe{(*recipient_id)}, message
        // );

        let batch =
            if self.out_batches.last().unwrap().len() < self.batch_message_bytes - message_size {
                self.out_batches.last_mut().unwrap()
            } else {
                self.out_batches
                    .push(Vec::with_capacity(self.batch_message_bytes));
                self.out_batches.last_mut().unwrap()
            };

        batch
            .write_u32::<LittleEndian>(message_size as u32)
            .unwrap();

        batch
    }

    pub fn try_send_pending(&mut self) -> Result<(), ::tungstenite::Error> {
        for batch in self.out_batches.drain(..) {
            match self
                .websocket
                .write_message(WebSocketMessage::binary(batch))
            {
                Ok(_) => {}
                Err(e) => if let Some(real_err) = e.into_non_blocking() {
                    return Err(real_err);
                },
            }
        }

        self.out_batches.push(Vec::with_capacity(self.batch_message_bytes));

        match self.websocket.write_pending() {
            Ok(()) => Ok(()),
            Err(e) => if let Some(real_err) = e.into_non_blocking() {
                Err(real_err)
            } else {
                Ok(())
            },
        }
    }

    pub fn try_receive(
        &mut self,
        inboxes: &mut [Option<Inbox>],
    ) -> Result<(), ::tungstenite::Error> {
        loop {
            let blocked = match self.websocket.read_message() {
                Ok(WebSocketMessage::Binary(data)) => dispatch_batch(
                    &data,
                    inboxes,
                    &mut self.n_turns,
                    &mut self.n_turns_since_own_turn,
                ),
                Ok(other_message) => panic!("Got a non binary message: {:?}", other_message),
                Err(e) => if let Some(real_err) = e.into_non_blocking() {
                    return Err(real_err);
                } else {
                    true
                },
            };

            if blocked {
                break;
            }
        }
        Ok(())
    }
}

fn dispatch_batch(
    data: &[u8],
    inboxes: &mut [Option<Inbox>],
    n_turns: &mut usize,
    n_turns_since_own_turn: &mut usize,
) -> bool {
    // let msg = format!("Got batch of len {}, {:?}", data.len(), data);
    // #[cfg(feature = "server")]
    // println!("{}", msg);
    // #[cfg(feature = "browser")]
    // console!(log, msg);

    let mut pos = 0;
    let mut one_wants_to_wait = false;

    while pos < data.len() {
        let message_size = LittleEndian::read_u32(&data[pos..]);
        pos += ::std::mem::size_of::<u32>();
        let wants_to_wait = dispatch_message(
            &data[pos..(pos + message_size as usize)],
            inboxes,
            n_turns,
            n_turns_since_own_turn,
        );
        one_wants_to_wait = one_wants_to_wait || wants_to_wait;

        pos += message_size as usize;
    }

    one_wants_to_wait
}

fn dispatch_message(
    data: &[u8],
    inboxes: &mut [Option<Inbox>],
    n_turns: &mut usize,
    n_turns_since_own_turn: &mut usize,
) -> bool {
    if data[0] == 0 && data[1] == 0 {
        // this is actually a turn start
        *n_turns = LittleEndian::read_u32(&data[::std::mem::size_of::<ShortTypeId>()..]) as usize;
        *n_turns_since_own_turn += 1;

        // pretend that we're blocked so we only ever process all
        // messages of 10 incoming turns within one of our own turns,
        // applying backpressure
        *n_turns_since_own_turn >= 10
    } else {
        let recipient_id =
            (&data[::std::mem::size_of::<ShortTypeId>()] as *const u8) as *const RawID;

        unsafe {
            // #[cfg(feature = "browser")]
            // {
            //     let debugmsg = format!(
            //         "Receiving packet for actor {:?}. Data: {:?}",
            //         (*recipient_id),
            //         data
            //     );
            //     console!(log, debugmsg);
            // }
            if let Some(ref mut inbox) = inboxes[(*recipient_id).type_id.as_usize()] {
                inbox.put_raw(&data);
            } else {
                // #[cfg(feature = "browser")]
                // {
                //     console!(error, "Yeah that didn't work (no inbox)")
                // }
                panic!(
                    "No inbox for {:?} (coming from network)",
                    (*recipient_id).type_id.as_usize()
                )
            }
        }

        false
    }
}

#[cfg(feature = "browser")]
use std::cell::RefCell;
#[cfg(feature = "browser")]
use std::collections::VecDeque;
#[cfg(feature = "browser")]
use std::rc::Rc;

#[cfg(feature = "browser")]
pub struct Connection {
    n_turns: usize,
    n_turns_since_own_turn: usize,
    websocket: WebSocket,
    in_queue: Rc<RefCell<VecDeque<Vec<u8>>>>,
    got_machine_id: Rc<RefCell<bool>>,
    out_batches: Vec<Vec<u8>>,
    batch_message_bytes: usize,
}

#[cfg(feature = "browser")]
use stdweb::web::event::SocketMessageEvent;

#[cfg(feature = "browser")]
impl Connection {
    pub fn new(websocket: WebSocket, batch_message_bytes: usize) -> Connection {
        let in_queue = Rc::new(RefCell::new(VecDeque::new()));
        let in_queue_for_listener = in_queue.clone();
        let got_machine_id = Rc::new(RefCell::new(false));
        let got_machine_id_for_listener = got_machine_id.clone();

        websocket.set_binary_type(SocketBinaryType::ArrayBuffer);
        websocket.add_event_listener(move |event: SocketMessageEvent| {
            let mut got_machine_id = got_machine_id_for_listener.borrow_mut();
            if *got_machine_id {
                in_queue_for_listener.borrow_mut().push_back({
                    let typed_array: TypedArray<u8> = event.data().into_array_buffer().unwrap().into();
                    typed_array.to_vec()
                })
            } else {
                // ignore first packet
                *got_machine_id = true;
            }
        });

        Connection {
            n_turns: 0,
            n_turns_since_own_turn: 0,
            websocket,
            in_queue,
            got_machine_id,
            out_batches: vec![Vec::with_capacity(batch_message_bytes)],
            batch_message_bytes,
        }
    }

    pub fn enqueue_in_batch(&mut self, message_size: usize) -> &mut Vec<u8> {
        // let recipient_id =
        //     (&message[::std::mem::size_of::<ShortTypeId>()] as *const u8) as *const RawID;
        // println!(
        //     "Enqueueing message recipient: {:?}, data: {:?}",
        //     unsafe{(*recipient_id)}, message
        // );

        let batch =
            if self.out_batches.last().unwrap().len() < self.batch_message_bytes - message_size {
                self.out_batches.last_mut().unwrap()
            } else {
                self.out_batches
                    .push(Vec::with_capacity(self.batch_message_bytes));
                self.out_batches.last_mut().unwrap()
            };

        batch
            .write_u32::<LittleEndian>(message_size as u32)
            .unwrap();

        batch
    }

    pub fn try_send_pending(&mut self) -> Result<(), ::std::io::Error> {
        if self.websocket.ready_state() == SocketReadyState::Open {
            for batch in self.out_batches.drain(..) {
                self.websocket.send_bytes(&batch).unwrap();
            }

            self.out_batches.push(Vec::with_capacity(self.batch_message_bytes));
        }
        Ok(())
    }

    pub fn try_receive(&mut self, inboxes: &mut [Option<Inbox>]) -> Result<(), ::std::io::Error> {
        if let Ok(mut in_queue) = self.in_queue.try_borrow_mut() {
            //console!(log, "Before drain!");
            for batch in in_queue.drain(..) {
                //console!(log, "Before dispatch!");
                dispatch_batch(
                    &batch,
                    inboxes,
                    &mut self.n_turns,
                    &mut self.n_turns_since_own_turn,
                );
                //console!(log, "After dispatch!")
            }
        } else {
            //console!(log, "Cannot borrow inqueue mutably!")
        }
        Ok(())
    }
}
