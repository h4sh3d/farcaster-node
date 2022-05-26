// LNP Node: node running lightning network protocol and generalized lightning
// channels.
// Written in 2020 by
//     Dr. Maxim Orlovsky <orlovsky@pandoracore.com>
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the MIT License
// along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

use std::time::{Duration, SystemTime};
use std::{collections::HashMap, sync::Arc};
use std::{rc::Rc, thread::spawn};

use amplify::Bipolar;
use bitcoin::secp256k1::rand::{self, Rng, RngCore};
use bitcoin::secp256k1::PublicKey;
use internet2::{addr::InetSocketAddr, CreateUnmarshaller, Unmarshall, Unmarshaller};
use internet2::{presentation, transport, zmqsocket, NodeAddr, TypedEnum, ZmqType, ZMQ_CONTEXT};
use lnp::p2p::legacy::{Messages, Ping};
use microservices::esb::{self, Handler};
use microservices::node::TryService;
use microservices::peer::{self, PeerConnection, PeerSender, SendMessage};

use crate::rpc::{
    request::{self, Msg, PeerInfo, TakeCommit, Token},
    Request, ServiceBus,
};
use crate::{CtlServer, Endpoints, Error, LogStyle, Service, ServiceConfig, ServiceId};

#[allow(clippy::too_many_arguments)]
pub fn run(
    config: ServiceConfig,
    connection: PeerConnection,
    internal_id: NodeAddr,
    local_id: PublicKey,
    remote_id: Option<PublicKey>,
    local_socket: Option<InetSocketAddr>,
    remote_socket: InetSocketAddr,
    connect: bool,
) -> Result<(), Error> {
    debug!("Splitting connection into receiver and sender parts");
    let (receiver, sender) = connection.split();

    debug!("Opening bridge between runtime and peer listener threads");
    let tx = ZMQ_CONTEXT.socket(zmq::PAIR)?;
    let rx = ZMQ_CONTEXT.socket(zmq::PAIR)?;
    tx.connect("inproc://bridge")?;
    rx.bind("inproc://bridge")?;

    let internal_identity = ServiceId::Peer(internal_id);

    debug!("Starting thread listening for messages from the remote peer");
    let bridge_handler = ListenerRuntime {
        internal_identity: internal_identity.clone(),
        bridge: esb::Controller::with(
            map! {
                ServiceBus::Bridge => esb::BusConfig {
                    carrier: zmqsocket::Carrier::Socket(tx),
                    router: None,
                    queued: true,
                }
            },
            BridgeHandler,
            ZmqType::Rep,
        )?,
    };
    let unmarshaller: Unmarshaller<Msg> = Msg::create_unmarshaller();
    let listener =
        peer::Listener::<ListenerRuntime, Msg>::with(receiver, bridge_handler, unmarshaller);
    spawn(move || listener.run_or_panic("peerd-listener"));
    // TODO: Use the handle returned by spawn to track the child process

    debug!(
        "Starting main service runtime with identity: {}",
        internal_identity
    );
    let runtime = Runtime {
        identity: internal_identity,
        local_id,
        remote_id,
        local_socket,
        remote_socket,
        routing: empty!(),
        sender,
        connect,
        started: SystemTime::now(),
        messages_sent: 0,
        messages_received: 0,
        awaited_pong: None,
    };
    let mut service = Service::service(config, runtime)?;
    service.add_loopback(rx)?;
    service.run_loop()?;
    unreachable!()
}

pub struct BridgeHandler;

impl esb::Handler<ServiceBus> for BridgeHandler {
    type Request = Request;
    type Error = Error;

    fn identity(&self) -> ServiceId {
        ServiceId::Loopback
    }

    fn handle(
        &mut self,
        _endpoints: &mut Endpoints,
        _bus: ServiceBus,
        _addr: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        // Bridge does not receive replies for now
        trace!("BridgeHandler received reply: {}", request);
        Ok(())
    }

    fn handle_err(&mut self, _: &mut Endpoints, err: esb::Error<ServiceId>) -> Result<(), Error> {
        // We simply propagate the error since it's already being reported
        Err(Error::Esb(err))
    }
}

pub struct ListenerRuntime {
    internal_identity: ServiceId,
    bridge: esb::Controller<ServiceBus, Request, BridgeHandler>,
}

impl ListenerRuntime {
    /// send msgs over bridge from remote to local runtime
    fn send_over_bridge(
        &mut self,
        req: <Unmarshaller<Msg> as Unmarshall>::Data,
    ) -> Result<(), Error> {
        debug!("Forwarding FWP message over BRIDGE interface to the runtime");
        self.bridge.send_to(
            ServiceBus::Bridge,
            self.internal_identity.clone(),
            Request::Protocol((&*req).clone()),
        )?;
        Ok(())
    }
}

use std::fmt::{Debug, Display};
impl peer::Handler<Msg> for ListenerRuntime {
    type Error = crate::Error;
    fn handle(
        &mut self,
        message: <Unmarshaller<Msg> as Unmarshall>::Data,
    ) -> Result<(), Self::Error> {
        // Forwarding all received messages to the runtime
        trace!("FWP message details: {:?}", message);
        self.send_over_bridge(message)
    }

    fn handle_err(&mut self, err: Self::Error) -> Result<(), Self::Error> {
        debug!("Underlying peer interface requested to handle {}", err);
        match err {
            Error::Peer(presentation::Error::Transport(transport::Error::TimedOut)) => {
                trace!("Time to ping the remote peer");
                // This means socket reading timeout and the fact that we need
                // to send a ping message
                //
                self.send_over_bridge(Arc::new(Msg::PingPeer))?;
                Ok(())
            }
            Error::Peer(presentation::Error::Transport(transport::Error::SocketIo(
                std::io::ErrorKind::UnexpectedEof,
            ))) => {
                error!(
                    "The remote peer has hung up, notifying that peerd has halted: {}",
                    err
                );
                self.send_over_bridge(Arc::new(Msg::PeerdShutdown))?;
                // park this thread, the process exit is supposed to be handled by the parent
                //  the socket will continue spamming this error until peerd is shutdown, this ensures it is only handled once
                std::thread::park();
                Ok(())
            }
            // for all other error types, indicating internal errors, we
            // propagate error to the upper level
            _ => {
                error!("Unrecoverable peer error {}, halting", err);
                Err(err)
            }
        }
    }
}

pub struct Runtime {
    identity: ServiceId,
    local_id: PublicKey,
    remote_id: Option<PublicKey>,
    local_socket: Option<InetSocketAddr>,
    remote_socket: InetSocketAddr,

    routing: HashMap<ServiceId, ServiceId>,
    sender: PeerSender,
    connect: bool,

    started: SystemTime,
    messages_sent: usize,
    messages_received: usize,
    awaited_pong: Option<u16>,
}

impl CtlServer for Runtime {}

impl esb::Handler<ServiceBus> for Runtime {
    type Request = Request;
    type Error = Error;

    fn identity(&self) -> ServiceId {
        self.identity.clone()
    }

    fn on_ready(&mut self, _endpoints: &mut Endpoints) -> Result<(), Error> {
        if self.connect {
            info!(
                "{} with the remote peer {}",
                "Initializing connection".bright_blue_bold(),
                self.remote_socket
            );
            // self.send_ctl(endpoints, ServiceId::Wallet, request::PeerSecret)

            // self.sender.send_message(Messages::Init(message::Init {
            //     global_features: none!(),
            //     local_features: none!(),
            //     assets: none!(),
            //     unknown_tlvs: none!(),
            // }))?;

            self.connect = false;
        }
        Ok(())
    }

    fn handle(
        &mut self,
        endpoints: &mut Endpoints,
        bus: ServiceBus,
        source: ServiceId,
        request: Request,
    ) -> Result<(), Self::Error> {
        match bus {
            ServiceBus::Msg => self.handle_rpc_msg(endpoints, source, request),
            ServiceBus::Ctl => self.handle_rpc_ctl(endpoints, source, request),
            ServiceBus::Bridge => self.handle_bridge(endpoints, source, request),
        }
    }

    fn handle_err(&mut self, _: &mut Endpoints, _: esb::Error<ServiceId>) -> Result<(), Error> {
        // We do nothing and do not propagate error; it's already being reported
        // with `error!` macro by the controller. If we propagate error here
        // this will make whole daemon panic
        Ok(())
    }
}

impl Runtime {
    /// send messages over the bridge
    fn handle_rpc_msg(
        &mut self,
        _endpoints: &mut Endpoints,
        _source: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        // match &request {
        //     Request::PeerMessage(Messages::FundingSigned(message::FundingSigned {
        //         channel_id,
        //         ..
        //     })) => {
        //         debug!(
        //             "Renaming channeld service from temporary id {:#} to channel id
        // #{:#}",             source, channel_id
        //         );
        //         self.routing.remove(&source);
        //         self.routing.insert(channel_id.clone().into(), source);
        //     }
        //     _ => {}
        // }
        match request.clone() {
            Request::Protocol(message) => {
                // 1. Check permissions
                // 2. Forward to the remote peer
                debug!("Message type: {}", message.get_type());
                debug!(
                    "Forwarding peer message to the remote peer, request: {}",
                    &request.get_type()
                );
                self.messages_sent += 1;
                self.sender.send_message(message)?;
            }
            _ => {
                error!("MSG RPC can be only used for forwarding Protocol Messages");
                return Err(Error::NotSupported(ServiceBus::Msg, request.get_type()));
            }
        }
        Ok(())
    }

    fn handle_rpc_ctl(
        &mut self,
        endpoints: &mut Endpoints,
        source: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        match request {
            Request::UpdateSwapId(channel_id) => {
                debug!(
                    "Renaming swapd service from temporary id {:#} to swap id #{:#}",
                    source, channel_id
                );
                self.routing.remove(&source);
                self.routing.insert(channel_id.into(), source);
            }
            Request::Terminate if source == ServiceId::Farcasterd => {
                info!("Terminating {}", self.identity().bright_white_bold());
                std::process::exit(0);
            }

            Request::GetInfo => {
                let info = PeerInfo {
                    local_id: self.local_id,
                    remote_id: self.remote_id.map(|id| vec![id]).unwrap_or_default(),
                    local_socket: self.local_socket,
                    remote_socket: vec![self.remote_socket],
                    uptime: SystemTime::now()
                        .duration_since(self.started)
                        .unwrap_or_else(|_| Duration::from_secs(0)),
                    since: self
                        .started
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_else(|_| Duration::from_secs(0))
                        .as_secs(),
                    messages_sent: self.messages_sent,
                    messages_received: self.messages_received,
                    connected: !self.connect,
                    awaits_pong: self.awaited_pong.is_some(),
                };
                self.send_ctl(endpoints, source, Request::PeerInfo(info))?;
            }

            _ => {
                error!("Request is not supported by the CTL interface");
                return Err(Error::NotSupported(ServiceBus::Ctl, request.get_type()));
            }
        }
        Ok(())
    }
    /// receive messages arriving over the bridge
    fn handle_bridge(
        &mut self,
        endpoints: &mut Endpoints,
        _source: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        debug!("BRIDGE RPC request: {}", request);

        if let Request::Protocol(_) = request {
            self.messages_received += 1;
        }

        match &request {
            Request::Protocol(Msg::PingPeer) => self.ping()?,

            Request::Protocol(Msg::Ping(Ping { pong_size, .. })) => self.pong(*pong_size)?,

            Request::Protocol(Msg::Pong(noise)) => {
                match self.awaited_pong {
                    None => error!("Unexpected pong from the remote peer"),
                    Some(len) if len as usize != noise.len() => {
                        warn!("Pong data size does not match requested with ping")
                    }
                    _ => trace!("Got pong reply, exiting pong await mode"),
                }
                self.awaited_pong = None;
            }

            Request::Protocol(Msg::PeerdShutdown) => {
                warn!("Exiting peerd");
                endpoints.send_to(
                    ServiceBus::Ctl,
                    self.identity(),
                    ServiceId::Farcasterd,
                    Request::PeerdTerminated,
                )?;
                std::process::exit(0);
            }

            // swap initiation message
            Request::Protocol(Msg::TakerCommit(_)) => {
                endpoints.send_to(
                    ServiceBus::Msg,
                    self.identity(),
                    ServiceId::Farcasterd,
                    request,
                )?;
            }
            Request::Protocol(msg) => {
                endpoints.send_to(
                    ServiceBus::Msg,
                    self.identity(),
                    ServiceId::Swap(msg.swap_id()),
                    request,
                )?;
            }
            // }
            // Request::PeerMessage(Messages::OpenChannel(_)) => {
            //     endpoints.send_to(
            //         ServiceBus::Msg,
            //         self.identity(),
            //         ServiceId::Farcasterd,
            //         request,
            //     )?;
            // }

            // Request::PeerMessage(Messages::AcceptChannel(accept_channel)) => {
            //     let channeld: ServiceId = accept_channel.temporary_channel_id.into();
            //     self.routing.insert(channeld.clone(), channeld.clone());
            //     endpoints.send_to(ServiceBus::Msg, self.identity(), channeld, request)?;
            // }

            // Request::PeerMessage(Messages::FundingCreated(message::FundingCreated {
            //     temporary_channel_id,
            //     ..
            // })) => {
            //     endpoints.send_to(
            //         ServiceBus::Msg,
            //         self.identity(),
            //         temporary_channel_id.clone().into(),
            //         request,
            //     )?;
            // }

            // Request::PeerMessage(Messages::FundingSigned(message::FundingSigned {
            //     channel_id,
            //     ..
            // }))
            // | Request::PeerMessage(Messages::FundingLocked(message::FundingLocked {
            //     channel_id,
            //     ..
            // }))
            // | Request::PeerMessage(Messages::UpdateAddHtlc(message::UpdateAddHtlc {
            //     channel_id,
            //     ..
            // }))
            // | Request::PeerMessage(Messages::UpdateFulfillHtlc(message::UpdateFulfillHtlc {
            //     channel_id,
            //     ..
            // }))
            // | Request::PeerMessage(Messages::UpdateFailHtlc(message::UpdateFailHtlc {
            //     channel_id,
            //     ..
            // }))
            // | Request::PeerMessage(Messages::UpdateFailMalformedHtlc(
            //     message::UpdateFailMalformedHtlc { channel_id, .. },
            // )) => {
            //     let channeld: ServiceId = channel_id.clone().into();
            //     endpoints.send_to(
            //         ServiceBus::Msg,
            //         self.identity(),
            //         self.routing.get(&channeld).cloned().unwrap_or(channeld),
            //         request,
            //     )?;
            // }

            // Request::PeerMessage(message) => {
            //     // 1. Check permissions
            //     // 2. Forward to the corresponding daemon
            //     debug!("Got peer FWP message {}", message);
            // }
            other => {
                error!("Request is not supported by the BRIDGE interface");
                dbg!(other);
                return Err(Error::NotSupported(ServiceBus::Bridge, request.get_type()));
            }
        }
        Ok(())
    }

    fn ping(&mut self) -> Result<(), Error> {
        trace!("Sending ping to the remote peer");
        if self.awaited_pong.is_some() {
            return Err(Error::NotResponding);
        }
        let mut rng = rand::thread_rng();
        let len: u16 = rng.gen_range(4, 32);
        let mut noise = vec![0u8; len as usize];
        rng.fill_bytes(&mut noise);
        let pong_size = rng.gen_range(4, 32);
        self.messages_sent += 1;
        self.sender.send_message(Msg::Ping(Ping {
            ignored: noise.into(),
            pong_size,
        }))?;
        self.awaited_pong = Some(pong_size);
        Ok(())
    }

    fn pong(&mut self, pong_size: u16) -> Result<(), Error> {
        trace!("Replying with pong to the remote peer");
        let mut rng = rand::thread_rng();
        let noise = vec![0u8; pong_size as usize]
            .iter()
            .map(|_| rng.gen())
            .collect();
        self.messages_sent += 1;
        self.sender.send_message(Msg::Pong(noise))?;
        Ok(())
    }
}
