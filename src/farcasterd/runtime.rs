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

use amplify::Wrapper;
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::ffi::OsStr;
use std::io;
use std::net::SocketAddr;
use std::process;
use std::time::{Duration, SystemTime};

use bitcoin::hashes::hex::ToHex;
use bitcoin::secp256k1;
use internet2::{NodeAddr, RemoteSocketAddr, TypedEnum};
use lnp::{
    message, ChannelId as SwapId, Messages, TempChannelId as TempSwapId,
};
use lnpbp::Chain;
use microservices::esb::{self, Handler};
use microservices::rpc::Failure;

use crate::rpc::request::{IntoProgressOrFalure, NodeInfo, OptionDetails};
use crate::rpc::{request, Request, ServiceBus};
use crate::{Config, Error, LogStyle, Service, ServiceId};

pub fn run(config: Config, node_id: secp256k1::PublicKey) -> Result<(), Error> {
    let runtime = Runtime {
        identity: ServiceId::Farcasterd,
        node_id,
        chain: config.chain.clone(),
        listens: none!(),
        started: SystemTime::now(),
        connections: none!(),
        swaps: none!(),
        spawning_services: none!(),
        opening_swaps: none!(),
        accepting_swaps: none!(),
    };

    Service::run(config, runtime, true)
}

pub struct Runtime {
    identity: ServiceId,
    node_id: secp256k1::PublicKey,
    chain: Chain,
    listens: HashSet<RemoteSocketAddr>,
    started: SystemTime,
    connections: HashSet<NodeAddr>,
    swaps: HashSet<SwapId>,
    spawning_services: HashMap<ServiceId, ServiceId>,
    opening_swaps: HashMap<ServiceId, request::CreateSwap>,
    accepting_swaps: HashMap<ServiceId, request::CreateSwap>,
}

impl esb::Handler<ServiceBus> for Runtime {
    type Request = Request;
    type Address = ServiceId;
    type Error = Error;

    fn identity(&self) -> ServiceId {
        self.identity.clone()
    }

    fn handle(
        &mut self,
        senders: &mut esb::SenderList<ServiceBus, ServiceId>,
        bus: ServiceBus,
        source: ServiceId,
        request: Request,
    ) -> Result<(), Self::Error> {
        match bus {
            ServiceBus::Msg => self.handle_rpc_msg(senders, source, request),
            ServiceBus::Ctl => self.handle_rpc_ctl(senders, source, request),
            _ => {
                Err(Error::NotSupported(ServiceBus::Bridge, request.get_type()))
            }
        }
    }

    fn handle_err(&mut self, _: esb::Error) -> Result<(), esb::Error> {
        // We do nothing and do not propagate error; it's already being reported
        // with `error!` macro by the controller. If we propagate error here
        // this will make whole daemon panic
        Ok(())
    }
}

impl Runtime {
    fn handle_rpc_msg(
        &mut self,
        _senders: &mut esb::SenderList<ServiceBus, ServiceId>,
        source: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        match request {
            Request::Hello => {
                // Ignoring; this is used to set remote identity at ZMQ level
            }

            // TODO put offer here

            Request::PeerMessage(Messages::OpenChannel(open_swap)) => {
                info!("Creating swap by peer request from {}", source);
                self.create_swap(source, None, open_swap, true)?;
            }

            Request::PeerMessage(_) => {
                // Ignore the rest of LN peer messages
            }

            _ => {
                error!(
                    "MSG RPC can be only used for forwarding LNPWP messages"
                );
                return Err(Error::NotSupported(
                    ServiceBus::Msg,
                    request.get_type(),
                ));
            }
        }
        Ok(())
    }

    fn handle_rpc_ctl(
        &mut self,
        senders: &mut esb::SenderList<ServiceBus, ServiceId>,
        source: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        let mut notify_cli = None;
        match request {
            Request::Hello => {
                // Ignoring; this is used to set remote identity at ZMQ level
                info!("{} daemon is {}", source.ended(), "connected".ended());

                match &source {
                    ServiceId::Farcasterd => {
                        error!(
                            "{}",
                            "Unexpected another farcasterd instance connection"
                                .err()
                        );
                    }
                    ServiceId::Peer(connection_id) => {
                        if self.connections.insert(connection_id.clone()) {
                            info!(
                                "Connection {} is registered; total {} \
                                 connections are known",
                                connection_id,
                                self.connections.len()
                            );
                        } else {
                            warn!(
                                "Connection {} was already registered; the \
                                 service probably was relaunched",
                                connection_id
                            );
                        }
                    }
                    ServiceId::Swaps(swap_id) => {
                        if self.swaps.insert(swap_id.clone()) {
                            info!(
                                "Swap {} is registered; total {} \
                                 swaps are known",
                                swap_id,
                                self.swaps.len()
                            );
                        } else {
                            warn!(
                                "Swap {} was already registered; the \
                                 service probably was relaunched",
                                swap_id
                            );
                        }
                    }
                    _ => {
                        // Ignoring the rest of daemon/client types
                    }
                }

                if let Some(swap_params) = self.opening_swaps.get(&source) {
                    // Tell swapd swap options and link it with the
                    // connection daemon
                    debug!(
                        "Daemon {} is known: we spawned it to create a swap. \
                         Ordering swap opening",
                        source
                    );
                    notify_cli = Some((
                        swap_params.report_to.clone(),
                        Request::Progress(format!(
                            "Swap daemon {} operational",
                            source
                        )),
                    ));
                    senders.send_to(
                        ServiceBus::Ctl,
                        self.identity(),
                        source.clone(),
                        Request::OpenSwapWith(swap_params.clone()),
                    )?;
                    self.opening_swaps.remove(&source);
                } else if let Some(swap_params) =
                    self.accepting_swaps.get(&source)
                {
                    // Tell swapd swap options and link it with the
                    // connection daemon
                    debug!(
                        "Daemon {} is known: we spawned it to create a swap. \
                         Ordering swap acceptance",
                        source
                    );
                    senders.send_to(
                        ServiceBus::Ctl,
                        self.identity(),
                        source.clone(),
                        Request::AcceptSwapFrom(swap_params.clone()),
                    )?;
                    self.accepting_swaps.remove(&source);
                } else if let Some(enquirer) =
                    self.spawning_services.get(&source)
                {
                    debug!(
                        "Daemon {} is known: we spawned it to create a new peer \
                         connection by a request from {}",
                        source, enquirer
                    );
                    notify_cli = Some((
                        Some(enquirer.clone()),
                        Request::Success(OptionDetails::with(format!(
                            "Peer connected to {}",
                            source
                        ))),
                    ));
                    self.spawning_services.remove(&source);
                }
            }

            Request::UpdateSwapId(new_id) => {
                debug!(
                    "Requested to update channel id {} on {}",
                    source, new_id
                );
                if let ServiceId::Swaps(old_id) = source {
                    if !self.swaps.remove(&old_id) {
                        warn!("Swap daemon {} was unknown", source);
                    }
                    self.swaps.insert(new_id);
                    debug!("Registered swap daemon id {}", new_id);
                } else {
                    error!(
                        "Swap id update may be requested only by a swapd, not {}", 
                        source
                    );
                }
            }

            Request::GetInfo => {
                senders.send_to(
                    ServiceBus::Ctl,
                    ServiceId::Farcasterd,
                    source,
                    Request::NodeInfo(NodeInfo {
                        node_id: self.node_id,
                        listens: self.listens.iter().cloned().collect(),
                        uptime: SystemTime::now()
                            .duration_since(self.started)
                            .unwrap_or(Duration::from_secs(0)),
                        since: self
                            .started
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or(Duration::from_secs(0))
                            .as_secs(),
                        peers: self.connections.iter().cloned().collect(),
                        swaps: self.swaps.iter().cloned().collect(),
                    }),
                )?;
            }

            Request::ListPeers => {
                senders.send_to(
                    ServiceBus::Ctl,
                    ServiceId::Farcasterd,
                    source,
                    Request::PeerList(
                        self.connections.iter().cloned().collect(),
                    ),
                )?;
            }

            Request::ListSwaps => {
                senders.send_to(
                    ServiceBus::Ctl,
                    ServiceId::Farcasterd,
                    source,
                    Request::SwapList(self.swaps.iter().cloned().collect()),
                )?;
            }

            Request::Listen(addr) => {
                let addr_str = addr.addr();
                if self.listens.contains(&addr) {
                    let msg = format!(
                        "Listener on {} already exists, ignoring request",
                        addr
                    );
                    warn!("{}", msg.err());
                    notify_cli = Some((
                        Some(source.clone()),
                        Request::Failure(Failure { code: 1, info: msg }),
                    ));
                } else {
                    self.listens.insert(addr);
                    info!(
                        "{} for incoming LN peer connections on {}",
                        "Starting listener".promo(),
                        addr_str
                    );
                    let resp = self.listen(addr);
                    match resp {
                    Ok(_) => info!("Connection daemon {} for incoming LN peer connections on {}", 
                                   "listens".ended(), addr_str),
                    Err(ref err) => error!("{}", err.err())
                }
                    senders.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd,
                        source.clone(),
                        resp.into_progress_or_failure(),
                    )?;
                    notify_cli = Some((
                        Some(source.clone()),
                        Request::Success(OptionDetails::with(format!(
                            "Node {} listens for connections on {}",
                            self.node_id, addr
                        ))),
                    ));
                }
            }

            Request::ConnectPeer(addr) => {
                info!(
                    "{} to remote peer {}",
                    "Connecting".promo(),
                    addr.promoter()
                );
                let resp = self.connect_peer(source.clone(), addr);
                match resp {
                    Ok(_) => {}
                    Err(ref err) => error!("{}", err.err()),
                }
                notify_cli = Some((
                    Some(source.clone()),
                    resp.into_progress_or_failure(),
                ));
            }

            Request::OpenSwapWith(request::CreateSwap {
                swap_req,
                peerd,
                report_to,
            }) => {
                info!(
                    "{} by request from {}",
                    "Creating channel".promo(),
                    source.promoter()
                );
                let resp = self.create_swap(peerd, report_to, swap_req, false);
                match resp {
                    Ok(_) => {}
                    Err(ref err) => error!("{}", err.err()),
                }
                notify_cli = Some((
                    Some(source.clone()),
                    resp.into_progress_or_failure(),
                ));
            }

            _ => {
                error!(
                    "{}",
                    "Request is not supported by the CTL interface".err()
                );
                return Err(Error::NotSupported(
                    ServiceBus::Ctl,
                    request.get_type(),
                ));
            }
        }

        if let Some((Some(respond_to), resp)) = notify_cli {
            senders.send_to(
                ServiceBus::Ctl,
                ServiceId::Farcasterd,
                respond_to,
                resp,
            )?;
        }

        Ok(())
    }

    fn listen(&mut self, addr: RemoteSocketAddr) -> Result<String, Error> {
        if let RemoteSocketAddr::Ftcp(inet) = addr {
            let socket_addr = SocketAddr::try_from(inet)?;
            let ip = socket_addr.ip();
            let port = socket_addr.port();

            debug!("Instantiating peerd...");

            // Start swapd
            let child = launch(
                "peerd",
                &["--listen", &ip.to_string(), "--port", &port.to_string()],
            )?;
            let msg = format!(
                "New instance of peerd launched with PID {}",
                child.id()
            );
            info!("{}", msg);
            Ok(msg)
        } else {
            Err(Error::Other(s!(
                "Only TCP is supported for now as an overlay protocol"
            )))
        }
    }

    fn connect_peer(
        &mut self,
        source: ServiceId,
        node_addr: NodeAddr,
    ) -> Result<String, Error> {
        debug!("Instantiating peerd...");

        // Start swapd
        let child = launch("peerd", &["--connect", &node_addr.to_string()])?;
        let msg =
            format!("New instance of peerd launched with PID {}", child.id());
        info!("{}", msg);

        self.spawning_services
            .insert(ServiceId::Peer(node_addr), source);
        debug!("Awaiting for peerd to connect...");

        Ok(msg)
    }

    // FIXME: swapify
    fn create_swap(
        &mut self,
        source: ServiceId,
        report_to: Option<ServiceId>,
        mut swap_req: message::OpenChannel,
        accept: bool,
    ) -> Result<String, Error> {
        debug!("Instantiating swapd...");

        // We need to initialize temporary channel id here
        if !accept {
            swap_req.temporary_channel_id = TempSwapId::random();
            debug!(
                "Generated {} as a temporary channel id",
                swap_req.temporary_channel_id
            );
        }

        // Start swapd
        let child = launch("swapd", &[swap_req.temporary_channel_id.to_hex()])?;
        let msg =
            format!("New instance of swapd launched with PID {}", child.id());
        info!("{}", msg);

        // Construct channel creation request
        let node_key = self.node_id;
        let swap_req = message::OpenChannel {
            chain_hash: self.chain.clone().chain_params().genesis_hash.into(),
            // TODO: Take these parameters from configuration
            push_msat: 0,
            dust_limit_satoshis: 0,
            max_htlc_value_in_flight_msat: 10000,
            channel_reserve_satoshis: 0,
            htlc_minimum_msat: 0,
            feerate_per_kw: 1,
            to_self_delay: 1,
            max_accepted_htlcs: 1000,
            funding_pubkey: node_key,
            revocation_basepoint: node_key,
            payment_point: node_key,
            delayed_payment_basepoint: node_key,
            htlc_basepoint: node_key,
            first_per_commitment_point: node_key,
            channel_flags: 1, // Announce the channel
            // shutdown_scriptpubkey: None,
            ..swap_req
        };

        let list = if accept {
            &mut self.accepting_swaps
        } else {
            &mut self.opening_swaps
        };
        list.insert(
            ServiceId::Swaps(SwapId::from_inner(
                swap_req.temporary_channel_id.into_inner(),
            )),
            request::CreateSwap {
                swap_req,
                peerd: source,
                report_to,
            },
        );
        debug!("Awaiting for swapd to connect...");

        Ok(msg)
    }
}

fn launch(
    name: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> io::Result<process::Child> {
    let mut bin_path = std::env::current_exe().map_err(|err| {
        error!("Unable to detect binary directory: {}", err);
        err
    })?;
    bin_path.pop();

    bin_path.push(name);
    #[cfg(target_os = "windows")]
    bin_path.set_extension("exe");

    debug!(
        "Launching {} as a separate process using `{}` as binary",
        name,
        bin_path.to_string_lossy()
    );

    let mut cmd = process::Command::new(bin_path);
    cmd.args(std::env::args().skip(1)).args(args);
    trace!("Executing `{:?}`", cmd);
    cmd.spawn().map_err(|err| {
        error!("Error launching {}: {}", name, err);
        err
    })
}