use crate::net::framed::AnyFramedTransport;
use crate::net::reliable::queue::MessageQueue;
use crate::net::reliable::{MessageId, UniqueMessage};
use futures::stream::StreamExt;
use futures::SinkExt;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, trace, warn};

/// Session ID derived from time of session creation
///
/// Session IDs are used to tie-break between different active sessions. They contain their creation
/// timestamp with milli second granularity. More recent sessions supersede older ones.
#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionId(u64);

#[derive(Debug, Serialize, Deserialize)]
pub enum SessionMessage<M> {
    Open(SessionId),
    Message(UniqueMessage<M>),
    HeartBeat(Option<MessageId>),
}

#[derive(Debug)]
enum SessionTaskControlMessage<M> {
    Close,
    SendMessage(M),
}

#[derive(Debug)]
pub struct Session<M> {
    io_task: JoinHandle<SessionState<M>>,
    sender: Sender<SessionTaskControlMessage<M>>,
    receiver: Receiver<M>,
}

#[derive(Debug)]
pub struct SessionState<M> {
    session_id: SessionId,
    last_received: Option<MessageId>,
    resend_buffer: MessageQueue<M>,
}

impl SessionId {
    /// Creates a new session ID from the current system time.
    pub fn new() -> SessionId {
        let time = std::time::SystemTime::now();
        let timestamp = time
            .duration_since(std::time::UNIX_EPOCH)
            .expect("UNIX epoch can be calculate")
            .as_millis() as u64; // Ok for a few 100 years

        SessionId(timestamp)
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl<M> Session<M>
where
    M: Debug + Clone + Serialize + DeserializeOwned + Unpin + Send + Sync + 'static,
{
    pub async fn new(
        state: SessionState<M>,
        connection: AnyFramedTransport<SessionMessage<M>>,
    ) -> Self {
        let (outgoing_sender, outgoing_receiver) = tokio::sync::mpsc::channel(1024);
        let (incoming_sender, incoming_receiver) = tokio::sync::mpsc::channel(1024);

        let io_task = tokio::spawn(Self::run_io_task(
            state,
            connection,
            outgoing_receiver,
            incoming_sender,
        ));

        Session {
            io_task,
            sender: outgoing_sender,
            receiver: incoming_receiver,
        }
    }

    async fn run_io_task(
        mut state: SessionState<M>,
        mut connection: AnyFramedTransport<SessionMessage<M>>,
        mut outgoing: Receiver<SessionTaskControlMessage<M>>,
        incoming: Sender<M>,
    ) -> SessionState<M> {
        debug!(sid = ?state.session_id, "Starting session io thread");

        let ack_duration = Duration::from_secs(2);
        let max_missed_acks = 3;
        let max_missed_ack_duration = ack_duration * max_missed_acks;

        let (conn_send, conn_receive) = connection.borrow_split();
        let mut heart_beat = tokio::time::interval(ack_duration);
        let mut expect_ack = Instant::now() + max_missed_ack_duration;

        // TODO: send HBs in parallel
        // Resend unconfirmed messages
        let mut send_error = false;
        for umsg in state.resend_buffer.iter() {
            trace!(session = ?state.session_id, message = ?umsg.id, "Resending message");
            if let Err(e) = conn_send.send(SessionMessage::Message(umsg.clone())).await {
                warn!(id = ?state.session_id, err = %e, "Session died while sending a message");
                send_error = true;
                break;
            };
        }
        if send_error {
            return state;
        }

        // Begin processing usual io
        loop {
            tokio::select! {
                msg = conn_receive.next() => {
                    match msg {
                        Some(Ok(SessionMessage::Message(umsg))) => {
                            let expected_msg_id = state.last_received
                                .map(MessageId::increment)
                                .unwrap_or(MessageId(1));
                            if umsg.id > expected_msg_id  {
                                warn!(
                                    id = ?state.session_id,
                                    expected_id = ?expected_msg_id,
                                    got_id = ?umsg.id,
                                    "Session received message with unexpected id, closing"
                                );
                                break;
                            } else if umsg.id != expected_msg_id {
                                trace!(
                                    id = ?state.session_id,
                                    expected_id = ?expected_msg_id,
                                    got_id = ?umsg.id,
                                    "Session received old message"
                                );
                                continue;
                            }

                            trace!(session = ?state.session_id, message = ?umsg.id, "Received message");
                            state.last_received = Some(umsg.id);
                            incoming
                                .send(umsg.msg)
                                .await
                                .expect("Session's owning connection went away unexpectedly");
                        },
                        Some(Ok(SessionMessage::HeartBeat(last_received))) => {
                            trace!(session = ?state.session_id, ack = ?last_received, "Received heart beat");
                            if let Some(msg_id) = last_received {
                                state.resend_buffer.ack(msg_id);
                            }
                            expect_ack = Instant::now() + max_missed_ack_duration;
                        }
                        None => {
                            warn!(id = ?state.session_id, "Session died while receiving");
                            break;
                        },
                        Some(Err(e)) => {
                            warn!(id = ?state.session_id, err = %e, "Session died while receiving");
                            break;
                        },
                        Some(Ok(msg)) => {
                            warn!(id = ?state.session_id, message = ?msg, "Session received unexpected message, closing");
                            break;
                        },
                    }
                },
                _ = heart_beat.tick() => {
                    trace!(session = ?state.session_id, "Sending heart beat");
                    if let Err(e) = conn_send.send(SessionMessage::HeartBeat(state.last_received)).await {
                        warn!(id = ?state.session_id, err = %e, "Session died while sending a heart beat");
                        break;
                    }
                },
                msg = outgoing.recv() => {
                    match msg.expect("Session's owning connection went away unexpectedly") {
                        SessionTaskControlMessage::SendMessage(msg) => {
                            let umsg = state.resend_buffer.push(msg);
                            trace!(session = ?state.session_id, message = ?umsg.id, "Sending message");
                            if let Err(e) = conn_send.send(SessionMessage::Message(umsg)).await {
                                warn!(id = ?state.session_id, err = %e, "Session died while sending a message");
                                break;
                            }
                        },
                        SessionTaskControlMessage::Close => {
                            trace!(session = ?state.session_id, "Received session close request");
                            break;}
                    }
                },
                _ = tokio::time::sleep_until(expect_ack) => {
                    // We didn't receive any ACK in the time we expected `max_missed_acks` ones
                    break;
                }
            };
        }

        outgoing.close();
        while let Some(cmsg) = outgoing.recv().await {
            if let SessionTaskControlMessage::SendMessage(msg) = cmsg {
                state.resend_buffer.push(msg);
            }
        }

        state
    }

    pub async fn receive(&mut self) -> Result<M, SessionTaskFinished> {
        self.check_io_task_running()?;
        self.receiver.recv().await.ok_or(SessionTaskFinished)
    }

    pub async fn send(&mut self, msg: M) -> Result<(), SessionTaskFinished> {
        self.check_io_task_running()?;

        let _ = self
            .sender
            .send(SessionTaskControlMessage::SendMessage(msg))
            .await;

        Ok(())
    }

    pub async fn close(mut self) -> (SessionState<M>, Vec<M>) {
        // We ignore errors in case the io task stopped by itself
        let _ = self.sender.send(SessionTaskControlMessage::Close).await;
        let state = self.io_task.await.expect("Session task died");

        // This probably never happens but could lead to weird bugs
        let mut remaining_messages = vec![];
        while let Some(msg) = self.receiver.recv().await {
            remaining_messages.push(msg);
        }

        (state, remaining_messages)
    }

    fn check_io_task_running(&self) -> Result<(), SessionTaskFinished> {
        if self.io_task.is_finished() {
            Err(SessionTaskFinished)
        } else {
            Ok(())
        }
    }
}

impl<M> Default for SessionState<M> {
    fn default() -> Self {
        SessionState {
            session_id: SessionId(0),
            last_received: None,
            resend_buffer: Default::default(),
        }
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
#[error("Session task finished, close the session to find out the cause")]
pub struct SessionTaskFinished;

#[cfg(test)]
mod tests {
    use crate::net::framed::{BidiFramed, FramedTransport};
    use crate::net::reliable::queue::MessageQueue;
    use crate::net::reliable::session::{Session, SessionId, SessionState, SessionTaskFinished};
    use crate::net::reliable::{MessageId, UniqueMessage};
    use std::time::Duration;
    use tokio::io::{DuplexStream, ReadHalf, WriteHalf};
    use tracing_subscriber::EnvFilter;

    async fn session_from_state(
        state_a: SessionState<u64>,
        state_b: SessionState<u64>,
    ) -> (Session<u64>, Session<u64>) {
        let (stream_a, stream_b) = tokio::io::duplex(1024);

        let framed_a =
            BidiFramed::<_, WriteHalf<DuplexStream>, ReadHalf<DuplexStream>>::new(stream_a)
                .to_any();
        let framed_b =
            BidiFramed::<_, WriteHalf<DuplexStream>, ReadHalf<DuplexStream>>::new(stream_b)
                .to_any();

        let mut session_a = Session::new(state_a, framed_a).await;
        let mut session_b = Session::new(state_b, framed_b).await;

        (session_a, session_b)
    }

    #[tokio::test]
    async fn test_roundtrip_close_all_acked() {
        let (mut session_a, mut session_b) =
            session_from_state(SessionState::default(), SessionState::default()).await;

        session_a.send(42).await.unwrap();
        let received = tokio::time::timeout(Duration::from_secs(1), session_b.receive())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received, 42);

        tokio::time::sleep(Duration::from_secs(3)).await;

        let (state_a, rem_a) = session_a.close().await;
        let (state_b, rem_b) = session_b.close().await;

        assert_eq!(state_a.last_received, None);
        assert!(state_a.resend_buffer.queue.is_empty());
        assert_eq!(state_a.resend_buffer.next_id, MessageId(2));
        assert_eq!(
            state_a.session_id,
            SessionState::<u64>::default().session_id
        );
        assert_eq!(rem_a, Vec::<u64>::new());

        assert_eq!(state_b.last_received, Some(MessageId(1)));
        assert_eq!(state_b.resend_buffer, MessageQueue::default());
        assert_eq!(
            state_b.session_id,
            SessionState::<u64>::default().session_id
        );
        assert_eq!(rem_b, Vec::<u64>::new());
    }

    #[tokio::test]
    async fn test_close_unfinished() {
        let mut alt_state = SessionState::default();
        alt_state.session_id = SessionId(1);
        let (mut session_a, mut session_b) =
            session_from_state(SessionState::default(), alt_state).await;

        session_a.send(42).await.unwrap();
        let received = tokio::time::timeout(Duration::from_secs(1), session_b.receive())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received, 42);

        session_a.send(21).await.unwrap();

        let (state_a, rem_a) = session_a.close().await;
        assert!(session_b.send(3).await.is_err());
        let (state_b, rem_b) = session_b.close().await;

        assert_eq!(state_a.last_received, None);
        assert_eq!(state_a.resend_buffer.next_id, MessageId(3));
        assert_eq!(
            &state_a.resend_buffer.queue,
            &[
                UniqueMessage {
                    id: MessageId(1),
                    msg: 42
                },
                UniqueMessage {
                    id: MessageId(2),
                    msg: 21
                }
            ]
        );
        assert_eq!(rem_a, Vec::<u64>::new());

        assert_eq!(state_b.last_received, Some(MessageId(2)));
        assert_eq!(state_b.resend_buffer.next_id, MessageId(1));
        assert_eq!(&state_b.resend_buffer.queue, &[]);
        assert_eq!(rem_b, vec![21u64]);
    }

    #[tokio::test]
    async fn test_reopen_unfinished() {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                    EnvFilter::new("info,minimint::net::reliable::session=trace")
                }),
            )
            .init();

        let state_a = SessionState {
            session_id: SessionId(0),
            last_received: None,
            resend_buffer: MessageQueue {
                queue: vec![
                    UniqueMessage {
                        id: MessageId(1),
                        msg: 42,
                    },
                    UniqueMessage {
                        id: MessageId(2),
                        msg: 21,
                    },
                ]
                .into(),
                next_id: MessageId(3),
            },
        };

        let state_b = SessionState {
            session_id: SessionId(1),
            last_received: Some(MessageId(1)),
            resend_buffer: MessageQueue {
                queue: vec![UniqueMessage {
                    id: MessageId(1),
                    msg: 3,
                }]
                .into(),
                next_id: MessageId(2),
            },
        };

        let (mut session_a, mut session_b) = session_from_state(state_a, state_b).await;

        session_a.send(5).await.unwrap();

        for msg in [21, 5] {
            let received = tokio::time::timeout(Duration::from_secs(1), session_b.receive())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(received, msg);
        }

        let received = tokio::time::timeout(Duration::from_secs(1), session_a.receive())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received, 3);
    }

    #[tokio::test]
    async fn test_timeout() {
        let (stream_a, stream_b) = tokio::io::duplex(1024);
        let framed_a =
            BidiFramed::<_, WriteHalf<DuplexStream>, ReadHalf<DuplexStream>>::new(stream_a)
                .to_any();
        let mut session_a = Session::<u64>::new(SessionState::default(), framed_a).await;

        let res = tokio::time::timeout(Duration::from_secs(10), session_a.receive())
            .await
            .expect("Session should have closed on its own");
        assert_eq!(res, Err(SessionTaskFinished));
    }
}
