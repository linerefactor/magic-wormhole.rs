use super::*;
use async_std::net::{TcpListener, TcpStream};
use futures::{AsyncReadExt, AsyncWriteExt, SinkExt, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
};
use transit::{TransitConnectError, TransitError};

const APPID_RAW: &str = "piegames.de/wormhole/port-forwarding";

/// The App ID associated with this protocol.
pub const APPID: AppID = AppID(Cow::Borrowed(APPID_RAW));

/// An [`crate::AppConfig`] with sane defaults for this protocol.
///
/// You **must not** change `id` and `rendezvous_url` to be interoperable.
/// The `app_version` can be adjusted if you want to disable some features.
pub const APP_CONFIG: crate::AppConfig<AppVersion> = crate::AppConfig::<AppVersion> {
    id: AppID(Cow::Borrowed(APPID_RAW)),
    rendezvous_url: Cow::Borrowed(crate::rendezvous::DEFAULT_RENDEZVOUS_SERVER),
    app_version: AppVersion {
        transit_abilities: transit::Abilities::ALL_ABILITIES,
        other: serde_json::Value::Null,
    },
};

/**
 * The application specific version information for this protocol.
 */
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppVersion {
    transit_abilities: transit::Abilities,
    #[serde(flatten)]
    other: serde_json::Value,
}

// TODO send peer errors when something went wrong (if possible)
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ForwardingError {
    #[error("Transfer was not acknowledged by peer")]
    AckError,
    #[error("Something went wrong on the other side: {}", _0)]
    PeerError(String),
    /// Some deserialization went wrong, we probably got some garbage
    #[error("Corrupt JSON message received")]
    ProtocolJson(
        #[from]
        #[source]
        serde_json::Error,
    ),
    /// Some deserialization went wrong, we probably got some garbage
    #[error("Corrupt Msgpack message received")]
    ProtocolMsgpack(
        #[from]
        #[source]
        rmp_serde::decode::Error,
    ),
    /// A generic string message for "something went wrong", i.e.
    /// the server sent some bullshit message order
    #[error("Protocol error: {}", _0)]
    Protocol(Box<str>),
    #[error(
        "Unexpected message (protocol error): Expected '{}', but got: {:?}",
        _0,
        _1
    )]
    ProtocolUnexpectedMessage(Box<str>, Box<dyn std::fmt::Debug + Send + Sync>),
    #[error("Wormhole connection error")]
    Wormhole(
        #[from]
        #[source]
        WormholeError,
    ),
    #[error("Error while establishing transit connection")]
    TransitConnect(
        #[from]
        #[source]
        TransitConnectError,
    ),
    #[error("Transit error")]
    Transit(
        #[from]
        #[source]
        TransitError,
    ),
    #[error("IO error")]
    IO(
        #[from]
        #[source]
        std::io::Error,
    ),
}

impl ForwardingError {
    fn protocol(message: impl Into<Box<str>>) -> Self {
        Self::Protocol(message.into())
    }

    pub(self) fn unexpected_message(
        expected: impl Into<Box<str>>,
        got: impl std::fmt::Debug + Send + Sync + 'static,
    ) -> Self {
        Self::ProtocolUnexpectedMessage(expected.into(), Box::new(got))
    }
}

pub async fn serve(
    mut wormhole: Wormhole,
    relay_hints: Vec<transit::RelayHint>,
    targets: Vec<(Option<url::Host>, u16)>,
) -> Result<(), ForwardingError> {
    let peer_version: AppVersion = serde_json::from_value(wormhole.peer_version.clone())?;
    let connector = transit::init(
        transit::Abilities::ALL_ABILITIES, // TODO use the ones we actually sent
        Some(peer_version.transit_abilities),
        relay_hints,
    )
    .await?;

    /* Send our transit hints */
    wormhole
        .send_json(&PeerMessage::Transit {
            hints: (**connector.our_hints()).clone(),
        })
        .await?;

    let targets: HashMap<String, (Option<url::Host>, u16)> = targets
        .into_iter()
        .map(|(host, port)| match host {
            Some(host) => {
                if port == 80 || port == 443 || port == 8000 || port == 8080 {
                    log::warn!("It seems like you are trying to forward a remote HTTP target ('{}'). Due to HTTP being host-aware this will very likely fail!", host);
                }
                (format!("{}:{}", host, port), (Some(host), port))
            },
            None => (port.to_string(), (host, port)),
        })
        .collect();

    /* Receive their transit hints */
    let their_hints: transit::Hints = match wormhole.receive_json().await?? {
        PeerMessage::Transit { hints } => {
            log::debug!("Received transit message: {:?}", hints);
            hints
        },
        PeerMessage::Error(err) => {
            bail!(ForwardingError::PeerError(err));
        },
        other => {
            let error = ForwardingError::unexpected_message("transit", other);
            let _ = wormhole
                .send_json(&PeerMessage::Error(format!("{}", error)))
                .await;
            bail!(error)
        },
    };

    let mut transit = match connector
        .leader_connect(
            wormhole.key().derive_transit_key(wormhole.appid()),
            peer_version.transit_abilities,
            Arc::new(their_hints),
        )
        .await
    {
        Ok(transit) => transit,
        Err(error) => {
            let error = ForwardingError::TransitConnect(error);
            let _ = wormhole
                .send_json(&PeerMessage::Error(format!("{}", error)))
                .await;
            return Err(error);
        },
    };

    /* We got a transit, now close the Wormhole */
    wormhole.close().await?;

    transit
        .send_record(
            &PeerMessage::Offer {
                addresses: targets.keys().cloned().collect(),
            }
            .ser_msgpack(),
        )
        .await?;

    let (backchannel_tx, backchannel_rx) =
        futures::channel::mpsc::channel::<(u64, Option<Vec<u8>>)>(20);

    let (transit_tx, transit_rx) = transit.split();
    let transit_rx = transit_rx.fuse();
    futures::pin_mut!(transit_tx);
    futures::pin_mut!(transit_rx);

    /* Main processing loop. Catch errors */
    let result = ForwardingServe {
        targets,
        connections: HashMap::new(),
        historic_connections: HashSet::new(),
        backchannel_tx,
        backchannel_rx,
    }
    .run(&mut transit_tx, &mut transit_rx)
    .await;
    /* If the error is not a PeerError (i.e. coming from the other side), try notifying the other side before quitting. */
    match result {
        Ok(()) => Ok(()),
        Err(error @ ForwardingError::PeerError(_)) => Err(error),
        Err(error) => {
            let _ = transit_tx
                .send(
                    PeerMessage::Error(format!("{}", error))
                        .ser_msgpack()
                        .into_boxed_slice(),
                )
                .await;
            Err(error)
        },
    }
}

struct ForwardingServe {
    targets: HashMap<String, (Option<url::Host>, u16)>,
    /* self => remote */
    connections: HashMap<
        u64,
        (
            async_std::task::JoinHandle<()>,
            futures::io::WriteHalf<TcpStream>,
        ),
    >,
    /* Track old connection IDs that won't be reused again. This is to distinguish race hazards where
     * one side closes a connection while the other one accesses it simultaneously. Despite the name, the
     * set also includes connections that are currently live.
     */
    historic_connections: HashSet<u64>,
    /* remote => self. (connection_id, Some=payload or None=close) */
    backchannel_tx: futures::channel::mpsc::Sender<(u64, Option<Vec<u8>>)>,
    backchannel_rx: futures::channel::mpsc::Receiver<(u64, Option<Vec<u8>>)>,
}

//futures::pin_mut!(backchannel_rx);
impl ForwardingServe {
    async fn forward(
        &mut self,
        transit_tx: &mut (impl futures::sink::Sink<Box<[u8]>, Error = TransitError> + Unpin),
        connection_id: u64,
        payload: &[u8],
    ) -> Result<(), ForwardingError> {
        log::debug!("Forwarding {} bytes from #{}", payload.len(), connection_id);
        match self.connections.get_mut(&connection_id) {
            Some((_worker, connection)) => {
                /* On an error, log for the user and then terminate that connection */
                if let Err(e) = connection.write_all(payload).await {
                    log::warn!("Forwarding to #{} failed: {}", connection_id, e);
                    self.remove_connection(transit_tx, connection_id, true)
                        .await?;
                }
            },
            None if !self.historic_connections.contains(&connection_id) => {
                bail!(ForwardingError::protocol(format!(
                    "Connection '{}' not found",
                    connection_id
                )));
            },
            None => { /* Race hazard. Do nothing. */ },
        }
        Ok(())
    }

    async fn remove_connection(
        &mut self,
        transit_tx: &mut (impl futures::sink::Sink<Box<[u8]>, Error = TransitError> + Unpin),
        connection_id: u64,
        tell_peer: bool,
    ) -> Result<(), ForwardingError> {
        log::debug!("Removing connection: #{}", connection_id);
        if tell_peer {
            transit_tx
                .send(
                    PeerMessage::Disconnect { connection_id }
                        .ser_msgpack()
                        .into_boxed_slice(),
                )
                .await?;
        }
        match self.connections.remove(&connection_id) {
            Some((worker, _connection)) => {
                worker.cancel().await;
            },
            None if !self.historic_connections.contains(&connection_id) => {
                bail!(ForwardingError::protocol(format!(
                    "Connection '{}' not found",
                    connection_id
                )));
            },
            None => { /* Race hazard. Do nothing. */ },
        }
        Ok(())
    }

    async fn spawn_connection(
        &mut self,
        transit_tx: &mut (impl futures::sink::Sink<Box<[u8]>, Error = TransitError> + Unpin),
        mut target: String,
        connection_id: u64,
    ) -> Result<(), ForwardingError> {
        log::debug!("Creating new connection: #{} -> {}", connection_id, target);

        use std::collections::hash_map::Entry;
        let entry = match self.connections.entry(connection_id) {
            Entry::Vacant(entry) => entry,
            Entry::Occupied(_) => {
                bail!(ForwardingError::protocol(format!(
                    "Connection '{}' already exists",
                    connection_id
                )));
            },
        };

        let (host, port) = self.targets.get(&target).unwrap();
        if host.is_none() {
            target = format!("[::1]:{}", port);
        }
        let stream = match TcpStream::connect(&target).await {
            Ok(stream) => stream,
            Err(err) => {
                log::warn!(
                    "Cannot open connection to {}: {}. The forwarded service might be down.",
                    target,
                    err
                );
                transit_tx
                    .send(
                        PeerMessage::Disconnect { connection_id }
                            .ser_msgpack()
                            .into_boxed_slice(),
                    )
                    .await?;
                return Ok(());
            },
        };
        let (mut connection_rd, connection_wr) = stream.split();
        let mut backchannel_tx = self.backchannel_tx.clone();
        let worker = async_std::task::spawn_local(async move {
            let mut buffer = vec![0; 4096];
            /* Ignore errors */
            macro_rules! break_on_err {
                ($expr:expr) => {
                    match $expr {
                        Ok(val) => val,
                        Err(_) => break,
                    }
                };
            }
            #[allow(clippy::while_let_loop)]
            loop {
                let read = break_on_err!(connection_rd.read(&mut buffer).await);
                if read == 0 {
                    break;
                }
                let buffer = &buffer[..read];
                break_on_err!(
                    backchannel_tx
                        .send((connection_id, Some(buffer.to_vec())))
                        .await
                );
            }
            /* Close connection (maybe or not because of error) */
            let _ = backchannel_tx.send((connection_id, None)).await;
            backchannel_tx.disconnect();
        });
        entry.insert((worker, connection_wr));
        Ok(())
    }

    async fn shutdown(self) {
        log::debug!("Shutting down everything");
        for (worker, _connection) in self.connections.into_values() {
            worker.cancel().await;
        }
    }

    async fn run(
        mut self,
        transit_tx: &mut (impl futures::sink::Sink<Box<[u8]>, Error = TransitError> + Unpin),
        transit_rx: &mut (impl futures::stream::FusedStream<Item = Result<Box<[u8]>, TransitError>>
                  + Unpin),
    ) -> Result<(), ForwardingError> {
        /* Event processing loop */
        log::debug!("Entered processing loop");
        loop {
            futures::select! {
                message = transit_rx.next() => {
                    match PeerMessage::de_msgpack(&message.unwrap()?)? {
                        PeerMessage::Forward { connection_id, payload } => {
                            self.forward(transit_tx, connection_id, &payload).await?
                        },
                        PeerMessage::Connect { target, connection_id } => {
                            /* No matter what happens, as soon as we receive the "connect" command that ID is burned. */
                            self.historic_connections.insert(connection_id);
                            ensure!(
                                self.targets.contains_key(&target),
                                ForwardingError::protocol(format!("We don't know forwarding target '{}'", target)),
                            );

                            self.spawn_connection(transit_tx, target, connection_id).await?;
                        },
                        PeerMessage::Disconnect { connection_id } => {
                            self.remove_connection(transit_tx, connection_id, false).await?;
                        },
                        PeerMessage::Close => {
                            self.shutdown().await;
                            break Ok(());
                        },
                        PeerMessage::Error(err) => {
                            self.shutdown().await;
                            bail!(ForwardingError::PeerError(err));
                        },
                        other => {
                            self.shutdown().await;
                            bail!(ForwardingError::unexpected_message("connect' or 'disconnect' or 'forward' or 'close", other));
                        },
                    }
                },
                message = self.backchannel_rx.next() => {
                    /* This channel will never run dry, since we always have at least one sender active */
                    match message.unwrap() {
                        (connection_id, Some(payload)) => {
                            transit_tx.send(
                                PeerMessage::Forward {
                                    connection_id,
                                    payload
                                }
                                .ser_msgpack()
                                .into_boxed_slice()
                            ).await?;
                        },
                        (connection_id, None) => {
                            self.remove_connection(transit_tx, connection_id, true).await?;
                        },
                    }
                },
            }
        }
    }
}

pub async fn connect(
    mut wormhole: Wormhole,
    relay_hints: Vec<transit::RelayHint>,
    bind_address: Option<std::net::IpAddr>,
    custom_ports: &[u16],
) -> Result<(), ForwardingError> {
    let peer_version: AppVersion = serde_json::from_value(wormhole.peer_version.clone())?;
    let connector = transit::init(
        transit::Abilities::ALL_ABILITIES, // TODO use the ones we actually sent
        Some(peer_version.transit_abilities),
        relay_hints,
    )
    .await?;
    let bind_address = bind_address.unwrap_or_else(|| std::net::IpAddr::V6("::".parse().unwrap()));

    /* Send our transit hints */
    wormhole
        .send_json(&PeerMessage::Transit {
            hints: (**connector.our_hints()).clone(),
        })
        .await?;

    /* Receive their transit hints */
    let their_hints: transit::Hints = match wormhole.receive_json().await?? {
        PeerMessage::Transit { hints } => {
            log::debug!("Received transit message: {:?}", hints);
            hints
        },
        PeerMessage::Error(err) => {
            bail!(ForwardingError::PeerError(err));
        },
        other => {
            let error = ForwardingError::unexpected_message("transit", other);
            let _ = wormhole
                .send_json(&PeerMessage::Error(format!("{}", error)))
                .await;
            bail!(error)
        },
    };

    let transit = match connector
        .follower_connect(
            wormhole.key().derive_transit_key(wormhole.appid()),
            peer_version.transit_abilities,
            Arc::new(their_hints),
        )
        .await
    {
        Ok(transit) => transit,
        Err(error) => {
            let error = ForwardingError::TransitConnect(error);
            let _ = wormhole
                .send_json(&PeerMessage::Error(format!("{}", error)))
                .await;
            return Err(error);
        },
    };

    /* We got a transit, now close the Wormhole */
    wormhole.close().await?;

    let (transit_tx, transit_rx) = transit.split();
    let transit_rx = transit_rx.fuse();
    futures::pin_mut!(transit_tx);
    futures::pin_mut!(transit_rx);

    /* Error handling catcher (see below) */
    let run = async {
        /* Receive offer and ask user */

        let addresses = match PeerMessage::de_msgpack(&transit_rx.next().await.unwrap()?)? {
            PeerMessage::Offer { addresses } => addresses,
            PeerMessage::Error(err) => {
                bail!(ForwardingError::PeerError(err));
            },
            other => {
                bail!(ForwardingError::unexpected_message("offer", other))
            },
        };

        // TODO ask user here
        // TODO proper port mapping

        /* self => remote
         *                  (address, connection)
         * Vec<Stream<Item = (String, TcpStream)>>
         */
        let listeners: Vec<(
            async_std::net::TcpListener,
            u16,
            std::rc::Rc<std::string::String>,
        )> = futures::stream::iter(
            addresses
                .iter()
                .cloned()
                .map(Rc::new)
                .zip(custom_ports.iter().copied().chain(std::iter::repeat(0))),
        )
        .then(|(address, port)| async move {
            let connection = TcpListener::bind((bind_address, port)).await?;
            let port = connection.local_addr()?.port();
            Result::<_, std::io::Error>::Ok((connection, port, address.clone()))
        })
        .try_collect()
        .await?;

        log::info!("Mapping the following open ports to targets:");
        log::info!("  local port -> remote target (no address = localhost on remote)");
        for (_, port, target) in &listeners {
            log::info!("  {} -> {}", port, target);
        }

        let (backchannel_tx, backchannel_rx) =
            futures::channel::mpsc::channel::<(u64, Option<Vec<u8>>)>(20);

        ForwardConnect {
            incoming: futures::stream::select_all(listeners.into_iter().map(
                |(connection, _, address)| {
                    connection
                        .into_incoming()
                        .map_ok(move |stream| (address.clone(), stream))
                        .boxed_local()
                },
            )),
            connection_counter: 0,
            connections: HashMap::new(),
            backchannel_tx,
            backchannel_rx,
        }
        .run(&mut transit_tx, &mut transit_rx)
        .await
    };

    match run.await {
        Ok(()) => Ok(()),
        Err(error @ ForwardingError::PeerError(_)) => Err(error),
        Err(error) => {
            let _ = transit_tx
                .send(
                    PeerMessage::Error(format!("{}", error))
                        .ser_msgpack()
                        .into_boxed_slice(),
                )
                .await;
            Err(error)
        },
    }
}

#[allow(clippy::type_complexity)]
struct ForwardConnect {
    //transit: &'a mut transit::Transit,
    /* when can I finally store an `impl Trait` in a struct? */
    incoming: futures::stream::SelectAll<
        futures::stream::LocalBoxStream<
            'static,
            Result<(Rc<String>, async_std::net::TcpStream), std::io::Error>,
        >,
    >,
    /* Our next unique connection_id */
    connection_counter: u64,
    connections: HashMap<
        u64,
        (
            async_std::task::JoinHandle<()>,
            futures::io::WriteHalf<TcpStream>,
        ),
    >,
    /* application => self. (connection_id, Some=payload or None=close) */
    backchannel_tx: futures::channel::mpsc::Sender<(u64, Option<Vec<u8>>)>,
    backchannel_rx: futures::channel::mpsc::Receiver<(u64, Option<Vec<u8>>)>,
}

impl ForwardConnect {
    async fn forward(
        &mut self,
        transit_tx: &mut (impl futures::sink::Sink<Box<[u8]>, Error = TransitError> + Unpin),
        connection_id: u64,
        payload: &[u8],
    ) -> Result<(), ForwardingError> {
        log::debug!("Forwarding {} bytes from #{}", payload.len(), connection_id);
        match self.connections.get_mut(&connection_id) {
            Some((_worker, connection)) => {
                /* On an error, log for the user and then terminate that connection */
                if let Err(e) = connection.write_all(payload).await {
                    log::warn!("Forwarding to #{} failed: {}", connection_id, e);
                    self.remove_connection(transit_tx, connection_id, true)
                        .await?;
                }
            },
            None if self.connection_counter <= connection_id => {
                bail!(ForwardingError::protocol(format!(
                    "Connection '{}' not found",
                    connection_id
                )));
            },
            None => { /* Race hazard. Do nothing. */ },
        }
        Ok(())
    }

    async fn remove_connection(
        &mut self,
        transit_tx: &mut (impl futures::sink::Sink<Box<[u8]>, Error = TransitError> + Unpin),
        connection_id: u64,
        tell_peer: bool,
    ) -> Result<(), ForwardingError> {
        log::debug!("Removing connection: #{}", connection_id);
        if tell_peer {
            transit_tx
                .send(
                    PeerMessage::Disconnect { connection_id }
                        .ser_msgpack()
                        .into_boxed_slice(),
                )
                .await?;
        }
        match self.connections.remove(&connection_id) {
            Some((worker, _connection)) => {
                worker.cancel().await;
            },
            None if connection_id >= self.connection_counter => {
                bail!(ForwardingError::protocol(format!(
                    "Connection '{}' not found",
                    connection_id
                )));
            },
            None => { /* Race hazard. Do nothing. */ },
        }
        Ok(())
    }

    async fn spawn_connection(
        &mut self,
        transit_tx: &mut (impl futures::sink::Sink<Box<[u8]>, Error = TransitError> + Unpin),
        target: Rc<String>,
        connection: TcpStream,
    ) -> Result<(), ForwardingError> {
        let connection_id = self.connection_counter;
        self.connection_counter += 1;
        let (mut connection_rd, connection_wr) = connection.split();
        let mut backchannel_tx = self.backchannel_tx.clone();
        log::debug!("Creating new connection: #{} -> {}", connection_id, target);

        transit_tx
            .send(
                PeerMessage::Connect {
                    target: (*target).clone(),
                    connection_id,
                }
                .ser_msgpack()
                .into_boxed_slice(),
            )
            .await?;

        let worker = async_std::task::spawn_local(async move {
            let mut buffer = vec![0; 4096];
            /* Ignore errors */
            macro_rules! break_on_err {
                ($expr:expr) => {
                    match $expr {
                        Ok(val) => val,
                        Err(_) => break,
                    }
                };
            }
            #[allow(clippy::while_let_loop)]
            loop {
                let read = break_on_err!(connection_rd.read(&mut buffer).await);
                if read == 0 {
                    break;
                }
                let buffer = &buffer[..read];
                break_on_err!(
                    backchannel_tx
                        .send((connection_id, Some(buffer.to_vec())))
                        .await
                );
            }
            /* Close connection (maybe or not because of error) */
            let _ = backchannel_tx.send((connection_id, None)).await;
            backchannel_tx.disconnect();
        });

        self.connections
            .insert(connection_id, (worker, connection_wr));
        Ok(())
    }

    async fn shutdown(self) {
        log::debug!("Shutting down everything");
        for (worker, _connection) in self.connections.into_values() {
            worker.cancel().await;
        }
    }

    async fn run(
        mut self,
        transit_tx: &mut (impl futures::sink::Sink<Box<[u8]>, Error = TransitError> + Unpin),
        transit_rx: &mut (impl futures::stream::FusedStream<Item = Result<Box<[u8]>, TransitError>>
                  + Unpin),
    ) -> Result<(), ForwardingError> {
        /* Event processing loop */
        log::debug!("Entered processing loop");
        loop {
            futures::select! {
                message = transit_rx.next() => {
                    match PeerMessage::de_msgpack(&message.unwrap()?)? {
                        PeerMessage::Forward { connection_id, payload } => {
                            self.forward(transit_tx, connection_id, &payload).await?;
                        },
                        PeerMessage::Disconnect { connection_id } => {
                            self.remove_connection(transit_tx, connection_id, false).await?;
                        },
                        PeerMessage::Close => {
                            self.shutdown().await;
                            break Ok(())
                        },
                        PeerMessage::Error(err) => {
                            for (worker, _connection) in self.connections.into_values() {
                                worker.cancel().await;
                            }
                            bail!(ForwardingError::PeerError(err));
                        },
                        other => {
                            self.shutdown().await;
                            bail!(ForwardingError::unexpected_message("connect' or 'disconnect' or 'forward' or 'close", other));
                        },
                    }
                },
                message = self.backchannel_rx.next() => {
                    /* This channel will never run dry, since we always have at least one sender active */
                    match message.unwrap() {
                        (connection_id, Some(payload)) => {
                            transit_tx.send(
                                PeerMessage::Forward {
                                    connection_id,
                                    payload
                                }.ser_msgpack()
                                .into_boxed_slice()
                            )
                            .await?;
                        },
                        (connection_id, None) => {
                            self.remove_connection(transit_tx, connection_id, true).await?;
                        },
                    }
                },
                connection = self.incoming.next() => {
                    let (target, connection): (Rc<String>, TcpStream) = connection.unwrap()?;
                    self.spawn_connection(transit_tx, target, connection).await?;
                }
            }
        }
    }
}

/** Serialization struct for this protocol */
#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
enum PeerMessage {
    /** Offer some destinations to be forwarded to.
     * forwarder -> forwardee only
     */
    Offer { addresses: Vec<String> },
    /** Forward a new connection.
     * forwardee -> forwarder only
     */
    Connect { target: String, connection_id: u64 },
    /** End a forwarded connection.
     * Any direction. Errors or the reason why the connection is closed
     * are not forwarded.
     */
    Disconnect { connection_id: u64 },
    /** Forward some bytes for a connection. */
    Forward {
        connection_id: u64,
        payload: Vec<u8>,
    },
    /** Close the whole session */
    Close,
    /** Tell the other side you got an error */
    Error(String),
    /** Used to set up a transit channel */
    Transit { hints: transit::Hints },
    #[serde(other)]
    Unknown,
}

impl PeerMessage {
    #[allow(dead_code)]
    pub fn ser_msgpack(&self) -> Vec<u8> {
        let mut writer = Vec::with_capacity(128);
        let mut ser = rmp_serde::encode::Serializer::new(&mut writer)
            .with_struct_map()
            .with_string_variants();
        serde::Serialize::serialize(self, &mut ser).unwrap();
        writer
    }

    #[allow(dead_code)]
    pub fn de_msgpack(data: &[u8]) -> Result<Self, rmp_serde::decode::Error> {
        rmp_serde::from_read(&mut &*data)
    }
}