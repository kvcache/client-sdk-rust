use std::{pin::{pin, Pin}, task::{Poll, Context}, time::Duration, collections::VecDeque, mem::take};

use futures::{Future, future::BoxFuture, FutureExt, StreamExt, stream::{SplitSink, SplitStream}, Stream, Sink};
use momento_protos::websocket::{SocketRequest, SocketResponse, self, socket_response::Status};
use prost::Message;
use tokio::{sync::{mpsc::{channel, Sender, Receiver}, oneshot}, net::TcpStream, task::JoinHandle};
use tokio_tungstenite::{WebSocketStream, MaybeTlsStream, tungstenite::{handshake::client::Response, client::IntoClientRequest, http::HeaderValue}, connect_async_with_config};

pub struct Headers {
    cache: HeaderValue,
    authorization: HeaderValue,
}
impl Headers {
    pub fn new(cache: String, authorization: String) -> Self {
        Self {
            cache: cache.parse().expect("cache name should be header-legal"),
            authorization: authorization.parse().expect("authorization should be header-legal"),
        }
    }
}

enum State {
    NotConnected {
        pending_commands: Option<Receiver<CommandSubmission>>,
    },
    WaitingToConnect {
        waiting: BoxFuture<'static, ()>,
        pending_commands: Option<Receiver<CommandSubmission>>,
    },
    Connecting {
        connecting: BoxFuture<'static, Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response), tokio_tungstenite::tungstenite::Error>>,
        pending_commands: Option<Receiver<CommandSubmission>>,
    },
    Streaming {
        write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, tokio_tungstenite::tungstenite::Message>,
        read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        current_command_id: u64,
        pending_commands: Option<Receiver<CommandSubmission>>,
        /// Trivially sorted by command id. Command id increases one-by-one, so later commands (toward the tail) will always have higher ids.
        in_flight_commands: VecDeque<CommandCompletion>,
    }
}

type CommandResult = websocket::socket_response::Kind;

struct CommandCompletion {
    complete: oneshot::Sender<CommandResult>,
    command_id: u64,
}

struct CommandSubmission {
    complete: oneshot::Sender<CommandResult>,
    command: websocket::socket_request::Kind,
}

impl CommandSubmission {
    fn new_submitted(command: websocket::socket_request::Kind) -> (Self, oneshot::Receiver<CommandResult>) {
        let (complete, notify) = oneshot::channel();
        (Self { complete, command }, notify)
    }
}

impl CommandCompletion {
    fn complete(self, response: websocket::socket_response::Kind) {
        match self.complete.send(response) {
            Ok(_) => (),
            Err(e) => {
                log::debug!("could not send completion. Did you drop the command already? {e:?}");
            }
        }
    }

    fn complete_cache(self, response: websocket::socket_response::CacheResponse) {
        self.complete(websocket::socket_response::Kind::Cache(response))
    }

    fn error(self, e: Status) {
        self.complete(websocket::socket_response::Kind::Error(e))
    }

    fn command_id(&self) -> u64 {
        self.command_id
    }
}

pub struct DataSocket {
    command_sender: Sender<CommandSubmission>,
    driver_task: JoinHandle<()>,
}

impl DataSocket {
    pub fn new(url: impl Into<hyper::Uri>, headers: Headers) -> Self {
        let (sender, receiver) = channel(64);
        let driver_task = tokio::spawn(
            DataSocketDriver {
                url: url.into(),
                headers,
                state: State::NotConnected { pending_commands: Some(receiver) },
            }
        );
        Self {
            command_sender: sender,
            driver_task,
        }
    }

    pub async fn run_cache_command(&self, command: websocket::socket_request::cache_request::Kind) -> Result<websocket::socket_response::cache_response::Kind, Status> {
        let (command, completion) = CommandSubmission::new_submitted(websocket::socket_request::Kind::Cache(websocket::socket_request::CacheRequest { kind: Some(command) }));
        match self.command_sender.send(command).await {
            Ok(_) => (),
            Err(e) => {
                return Err(Status { code: tonic::Code::Unavailable as i32, message: format!("could not send request: {e:?}") })
            }
        }
        match completion.await {
            Ok(result_kind) => {
                match result_kind {
                    websocket::socket_response::Kind::Error(e) => {
                        Err(e)
                    }
                    websocket::socket_response::Kind::Cache(cache_response) => {
                        match cache_response.kind {
                            Some(response_kind) => Ok(response_kind),
                            None => Err(Status { code: tonic::Code::DataLoss as i32, message: "no response for command".to_string() }),
                        }
                    }
                }
            }
            Err(receiver_error) => {
                Err(Status { code: tonic::Code::Unknown as i32, message: format!("could not get request: {receiver_error:?}") })
            }
        }
    }
}

impl Drop for DataSocket {
    fn drop(&mut self) {
        self.driver_task.abort();
    }
}

/// Spawn this on the runtime you're using.
struct DataSocketDriver {
    url: hyper::Uri,
    headers: Headers,
    state: State,
}

impl DataSocketDriver {
    fn set_reconnecting_state(&mut self, delay: Duration) {
        let pending_commands = self.take_pending_commands();
        self.state = State::WaitingToConnect { waiting: tokio::time::sleep(delay).boxed(), pending_commands: Some(pending_commands) }
    }

    fn set_connecting_state(&mut self) {
        let pending_commands = self.take_pending_commands();

        let mut client_request = (&self.url).into_client_request().expect("the uri should still be valid");
        client_request.headers_mut().insert("authorization", self.headers.authorization.clone());
        client_request.headers_mut().insert("cache", self.headers.cache.clone());
        client_request.headers_mut().insert("content-encoding", HeaderValue::from_static("proto"));
    
        let connection_future = connect_async_with_config(self.url.clone(), None, false);
        self.state = State::Connecting { connecting: connection_future.boxed(), pending_commands: Some(pending_commands) }
    }

    /// This is super unsafe and you should not call it directly. It should only be combined with a state setter function like set_reconnecting_state.
    fn take_pending_commands(&mut self) -> Receiver<CommandSubmission> {
        let pending_commands = match &mut self.state {
            State::NotConnected { ref mut pending_commands } => pending_commands,
            State::WaitingToConnect { waiting: _, ref mut pending_commands } => pending_commands,
            State::Connecting { connecting: _, pending_commands } => pending_commands,
            State::Streaming { write: _, read: _, current_command_id: _, pending_commands, ref mut in_flight_commands } => {
                for pending in in_flight_commands.drain(..) {
                    pending.error(Status { code: tonic::Code::Aborted as i32, message: "command stream interrupted".to_string() })
                }
                pending_commands
            }
        };
        take(pending_commands).expect("pending commands must be nonempty")
    }
}

impl Future for DataSocketDriver {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            match self.state {
                State::NotConnected { .. } => {
                    self.set_connecting_state();
                    // Each arm must make a choice: [a] Continue to the next loop, [b] break out/early exit
                    continue;
                }
                State::WaitingToConnect { ref mut waiting, .. } => {
                    match pin!(waiting).poll(context) {
                        Poll::Ready(_) => {
                            self.set_connecting_state();
                            continue;
                        }
                        Poll::Pending => break Poll::Pending,
                    }
                }
                State::Connecting { ref mut connecting, ref mut pending_commands } => {
                    match pin!(connecting).poll(context) {
                        Poll::Ready(connection_result) => {
                            match connection_result {
                                Ok((websocket, _response)) => {
                                    let (write, read) = websocket.split();
                                    self.state = State::Streaming { write, read, current_command_id: 0, in_flight_commands: VecDeque::with_capacity(128), pending_commands: Some(take(pending_commands).expect("pending commands must be nonempty")) };
                                    continue;
                                }
                                Err(e) => {
                                    log::debug!("failed to connect: {e:?}");
                                    self.set_reconnecting_state(Duration::from_millis(500));
                                    continue;
                                }
                            }
                        }
                        Poll::Pending => break Poll::Pending,
                    }
                }
                State::Streaming { ref mut write, ref mut read, in_flight_commands: ref mut commands, ref mut current_command_id, ref mut pending_commands } => {
                    // In streaming state, in addition to [a] and [b] choices, each sub-state machine has to make a choice as to
                    // whether it should redrive the state loop (e.g., on a state change) or allow the handler workflow to
                    // move forward.

                    // process responses
                    match pin!(read).poll_next(context) {
                        Poll::Ready(next) => {
                            match next {
                                Some(socket_result) => {
                                    match socket_result {
                                        Ok(message) => {
                                            if message.is_text() {
                                                log::warn!("got unknown response: {}", message.into_text().expect("checked before accessing"));
                                                self.set_reconnecting_state(Duration::from_millis(500))
                                            } else if !message.is_binary() {
                                                log::debug!("got non-message response")
                                            } else {
                                                match SocketResponse::decode(message.into_data().as_slice()) {
                                                    Ok(response) => {
                                                        let request_id = response.request_id; // need to map back to the completion for this request
                                                        match commands.binary_search_by_key(&request_id, |completion| completion.command_id()) {
                                                            Ok(complete_command_index) => {
                                                                let complete_command = commands.remove(complete_command_index).expect("search found this, so it should be there");
                                                                match response.kind {
                                                                    Some(response_kind) => {
                                                                        match response_kind {
                                                                            websocket::socket_response::Kind::Error(e) => {
                                                                                complete_command.error(e)
                                                                            }
                                                                            websocket::socket_response::Kind::Cache(cache_completion) => {
                                                                                complete_command.complete_cache(cache_completion)
                                                                            }
                                                                        }
                                                                    }
                                                                    None => {
                                                                        complete_command.error(Status { code: tonic::Code::Unknown as i32, message: "invalid response from service".to_string() })
                                                                    }
                                                                }
                                                            }
                                                            Err(_) => {
                                                                log::debug!("received response for unknown command")
                                                            }
                                                        }
                                                    }
                                                    Err(e) => {
                                                        log::error!("invalid message received: {e:?}");
                                                        self.set_reconnecting_state(Duration::from_millis(500))
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            log::debug!("socket error: {e:?}");
                                            self.set_reconnecting_state(Duration::from_millis(10))
                                        }
                                    }
                                }
                                None => {
                                    self.set_reconnecting_state(Duration::from_millis(50))
                                }
                            }
                            // No matter what, we're either transitioning state or we need to consider another command to complete.
                            // Let's re-drive until we're done dispatching results.
                            continue;
                        }
                        Poll::Pending => (), // we're registered for wake whenever the stream sends us something. Let's fall through to the next step.
                    }

                    // discover new commands
                    // If ready is not ready, that's backpressure from the network!
                    match pin!(&mut *write).poll_ready(context) {
                        Poll::Ready(state) => {
                            // Socket has a determined state...
                            match state {
                                Ok(_) => {
                                    // Socket can receive more data.
                                    match pending_commands.as_mut().expect("pending commands must be nonempty").poll_recv(context) {
                                        Poll::Ready(message) => {
                                            match message {
                                                Some(submission) => {
                                                    let request_id = *current_command_id;
                                                    *current_command_id += 1;
                                                    let command = SocketRequest {
                                                        request_id,
                                                        kind: Some(submission.command),
                                                    };
                                                    let websocket_message = tokio_tungstenite::tungstenite::Message::Binary(command.encode_to_vec());
                                                    // use feed so we can try to collapse system calls
                                                    match pin!(&mut *write).start_send(websocket_message) {
                                                        Ok(_) => {
                                                            commands.push_back(CommandCompletion { complete: submission.complete, command_id: request_id })
                                                        }
                                                        Err(e) => {
                                                            log::error!("websocket error, reconnecting: {e:?}");
                                                            self.set_reconnecting_state(Duration::from_millis(500));
                                                            continue;
                                                        }
                                                    }
                                                }
                                                None => {
                                                    log::debug!("sender was dropped - there is nothing left to do");
                                                    return Poll::Ready(()) // drop the stream
                                                }
                                            }
                                        }
                                        Poll::Pending => {
                                            // waiting for more commands to send - the socket has room when one arrives
                                            break Poll::Pending;
                                        }
                                    }
                                }
                                Err(e) => {
                                    log::error!("broken websocket: {e:?}");
                                    self.set_reconnecting_state(Duration::from_millis(50));
                                    continue;
                                }
                            }
                        }
                        Poll::Pending => {
                            // Sending data quickly, or we have a slow/problematic network. Either way, we can't
                            // send more yet, so let's park until we can send.
                            break Poll::Pending;
                        }
                    }
                }
            }
        }
    }
}
