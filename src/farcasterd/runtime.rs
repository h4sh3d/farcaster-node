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

use crate::farcasterd::runtime::request::{
    CheckpointEntry, MadeOffer, OfferStatus, OfferStatusPair, OfferStatusSelector, ProgressEvent,
    SwapProgress, TookOffer,
};
use crate::syncerd::{Event, SweepAddress, SweepAddressAddendum, Task, TaskId};
use crate::{
    clap::Parser,
    error::SyncerError,
    rpc::request::{
        BitcoinAddress, BitcoinFundingInfo, FundingInfo, Keys, LaunchSwap, MoneroAddress,
        MoneroFundingInfo, OfferInfo, Outcome, PubOffer, Token,
    },
    service::Endpoints,
    swapd::get_swap_id,
};
use bitcoin::hashes::Hash as BitcoinHash;
use clap::IntoApp;
use request::{Commit, List, Params};
use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::iter::FromIterator;
use std::process;
use std::time::{Duration, SystemTime};
use uuid::Uuid;

use bitcoin::{
    hashes::hex::ToHex,
    secp256k1::{PublicKey, SecretKey},
};
use internet2::{
    addr::NodeAddr,
    addr::{InetSocketAddr, NodeId},
    TypedEnum,
};
use microservices::esb::{self, Handler};

use farcaster_core::{
    blockchain::{Blockchain, Network},
    swap::SwapId,
};

use crate::farcasterd::Opts;
use crate::rpc::request::{Failure, FailureCode, GetKeys, Msg, NodeInfo};
use crate::rpc::{request, Request, ServiceBus};
use crate::{Config, CtlServer, Error, LogStyle, Service, ServiceConfig, ServiceId};

use farcaster_core::{role::TradeRole, swap::btcxmr::PublicOffer};

use std::str::FromStr;

pub fn run(
    service_config: ServiceConfig,
    config: Config,
    _opts: Opts,
    wallet_token: Token,
) -> Result<(), Error> {
    let _walletd = launch("walletd", &["--token", &wallet_token.to_string()])?;
    if config.is_grpc_enable() {
        let _grpcd = launch(
            "grpcd",
            &[
                "--grpc-port",
                &config
                    .farcasterd
                    .clone()
                    .unwrap()
                    .grpc
                    .unwrap()
                    .port
                    .to_string(),
            ],
        )?;
    }
    let empty: Vec<String> = vec![];
    let _databased = launch("databased", empty)?;

    if config.is_auto_funding_enable() {
        info!("farcasterd will attempt to fund automatically");
    }

    let runtime = Runtime {
        identity: ServiceId::Farcasterd,
        walletd_ready: false,
        databased_ready: false,
        node_secret_key: None,
        node_public_key: None,
        listens: none!(),
        started: SystemTime::now(),
        connections: none!(),
        report_peerd_reconnect: none!(),
        running_swaps: none!(),
        spawning_services: none!(),
        making_swaps: none!(),
        taking_swaps: none!(),
        pending_swap_init: none!(),
        arb_addrs: none!(),
        acc_addrs: none!(),
        public_offers: none!(),
        node_ids: none!(),
        peerd_ids: none!(),
        wallet_token,
        pending_sweep_requests: none!(),
        syncers: none!(),
        consumed_offers: none!(),
        progress: none!(),
        progress_subscriptions: none!(),
        stats: none!(),
        funding_xmr: none!(),
        funding_btc: none!(),
        checkpointed_pub_offers: vec![].into(),
        restoring_swap_id: none!(),
        config,
        syncer_task_counter: 0,
        syncer_tasks: none!(),
    };

    let broker = true;
    Service::run(service_config, runtime, broker)
}

pub struct Runtime {
    identity: ServiceId,
    walletd_ready: bool,
    databased_ready: bool,
    node_secret_key: Option<SecretKey>,
    node_public_key: Option<PublicKey>,
    listens: HashMap<Uuid, InetSocketAddr>,
    started: SystemTime,
    connections: HashSet<NodeAddr>,
    report_peerd_reconnect: HashMap<NodeAddr, ServiceId>,
    running_swaps: HashSet<SwapId>,
    spawning_services: HashMap<ServiceId, ServiceId>,
    making_swaps: HashMap<ServiceId, (request::InitSwap, Network)>,
    taking_swaps: HashMap<ServiceId, (request::InitSwap, Network)>,
    pending_swap_init: HashMap<(ServiceId, ServiceId), Vec<(Request, SwapId)>>,
    public_offers: HashSet<PublicOffer>,
    arb_addrs: HashMap<Uuid, bitcoin::Address>,
    acc_addrs: HashMap<Uuid, monero::Address>,
    consumed_offers: HashMap<PublicOffer, (SwapId, ServiceId)>,
    node_ids: HashMap<Uuid, NodeId>, // Only populated by maker. TODO is it possible? HashMap<SwapId, PublicKey>
    peerd_ids: HashMap<Uuid, ServiceId>, // Only populated by maker.
    wallet_token: Token,
    pending_sweep_requests: HashMap<ServiceId, Request>, // TODO: merge this with pending requests eventually
    syncers: Syncers,
    progress: HashMap<ServiceId, VecDeque<Request>>,
    progress_subscriptions: HashMap<ServiceId, HashSet<ServiceId>>,
    funding_btc: HashMap<SwapId, (bitcoin::Address, bitcoin::Amount, bool)>,
    funding_xmr: HashMap<SwapId, (monero::Address, monero::Amount, bool)>,
    checkpointed_pub_offers: List<CheckpointEntry>,
    restoring_swap_id: HashSet<SwapId>,
    stats: Stats,
    config: Config,
    syncer_task_counter: u32,
    syncer_tasks: HashMap<TaskId, ServiceId>,
}

impl CtlServer for Runtime {}

#[derive(Default)]
struct Stats {
    success: u64,
    refund: u64,
    punish: u64,
    abort: u64,
    initialized: u64,
    awaiting_funding_btc: u64,
    awaiting_funding_xmr: u64,
    funded_xmr: u64,
    funded_btc: u64,
    funding_canceled_xmr: u64,
    funding_canceled_btc: u64,
}

impl Stats {
    fn incr_outcome(&mut self, outcome: &Outcome) {
        match outcome {
            Outcome::Buy => self.success += 1,
            Outcome::Refund => self.refund += 1,
            Outcome::Punish => self.punish += 1,
            Outcome::Abort => self.abort += 1,
        };
    }
    fn incr_initiated(&mut self) {
        self.initialized += 1;
    }
    fn incr_awaiting_funding(&mut self, blockchain: &Blockchain) {
        match blockchain {
            Blockchain::Monero => self.awaiting_funding_xmr += 1,
            Blockchain::Bitcoin => self.awaiting_funding_btc += 1,
        }
    }
    fn incr_funded(&mut self, blockchain: &Blockchain) {
        match blockchain {
            Blockchain::Monero => {
                self.funded_xmr += 1;
                self.awaiting_funding_xmr -= 1;
            }
            Blockchain::Bitcoin => {
                self.funded_btc += 1;
                self.awaiting_funding_btc -= 1;
            }
        }
    }
    fn incr_funding_monero_canceled(&mut self) {
        self.awaiting_funding_xmr -= 1;
        self.funding_canceled_xmr += 1;
    }
    fn incr_funding_bitcoin_canceled(&mut self) {
        self.awaiting_funding_btc -= 1;
        self.funding_canceled_btc += 1;
    }
    fn success_rate(&self) -> f64 {
        let Stats {
            success,
            refund,
            punish,
            abort,
            initialized,
            awaiting_funding_btc,
            awaiting_funding_xmr,
            funded_btc,
            funded_xmr,
            funding_canceled_xmr,
            funding_canceled_btc,
        } = self;
        let total = success + refund + punish + abort;
        let rate = *success as f64 / (total as f64);
        info!(
            "Swapped({}) | Refunded({}) / Punished({}) | Aborted({}) | Initialized({}) / AwaitingFundingXMR({}) / AwaitingFundingBTC({}) / FundedXMR({}) / FundedBTC({}) / FundingCanceledXMR({}) / FundingCanceledBTC({})",
            success.bright_white_bold(),
            refund.bright_white_bold(),
            punish.bright_white_bold(),
            abort.bright_white_bold(),
            initialized,
            awaiting_funding_xmr.bright_white_bold(),
            awaiting_funding_btc.bright_white_bold(),
            funded_xmr.bright_white_bold(),
            funded_btc.bright_white_bold(),
            funding_canceled_xmr.bright_white_bold(),
            funding_canceled_btc.bright_white_bold(),
        );
        info!(
            "{} = {:>4.3}%",
            "Swap success".bright_blue_bold(),
            (rate * 100.).bright_yellow_bold(),
        );
        rate
    }
}

impl esb::Handler<ServiceBus> for Runtime {
    type Request = Request;
    type Error = Error;

    fn identity(&self) -> ServiceId {
        self.identity.clone()
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
            _ => Err(Error::NotSupported(ServiceBus::Bridge, request.get_type())),
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
    fn services_ready(&self) -> Result<(), Error> {
        if !self.walletd_ready {
            Err(Error::Farcaster(
                "Farcaster not ready yet, walletd still starting".to_string(),
            ))
        } else if !self.databased_ready {
            Err(Error::Farcaster(
                "Farcaster not ready yet, databased still starting".to_string(),
            ))
        } else {
            Ok(())
        }
    }
    fn peer_keys_ready(&self) -> Result<(SecretKey, PublicKey), Error> {
        if let (Some(sk), Some(pk)) = (self.node_secret_key, self.node_public_key) {
            Ok((sk, pk))
        } else {
            Err(Error::Farcaster("Peer keys not ready yet".to_string()))
        }
    }
    fn clean_up_after_swap(
        &mut self,
        swapid: &SwapId,
        endpoints: &mut Endpoints,
    ) -> Result<(), Error> {
        if self.running_swaps.remove(swapid) {
            endpoints.send_to(
                ServiceBus::Ctl,
                self.identity(),
                ServiceId::Swap(*swapid),
                Request::Terminate,
            )?;
        }
        endpoints.send_to(
            ServiceBus::Ctl,
            self.identity(),
            ServiceId::Database,
            Request::RemoveCheckpoint(*swapid),
        )?;
        let mut offerid = None;
        self.consumed_offers = self
            .consumed_offers
            .drain()
            .filter_map(|(k, (swap_id, service_id))| {
                if swapid != &swap_id {
                    Some((k, (swap_id, service_id)))
                } else {
                    offerid = Some(k.offer.id());
                    None
                }
            })
            .collect();
        let identity = self.identity();
        if let Some(offerid) = &offerid {
            if self.listens.contains_key(offerid) && self.node_ids.contains_key(offerid) {
                self.peerd_ids.remove(offerid);
                let node_id = self.node_ids.remove(offerid).unwrap();
                let remote_addr = self.listens.remove(offerid).unwrap();
                // nr of offers using that peerd
                if self
                    .listens
                    .values()
                    .filter(|x| x == &&remote_addr)
                    .into_iter()
                    .count()
                    == 0
                {
                    let connectionid = NodeAddr::new(node_id, remote_addr);

                    if self.connections.remove(&connectionid) {
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            identity.clone(),
                            ServiceId::Peer(connectionid),
                            Request::Terminate,
                        )?;
                    }
                }
            }
        }

        let client_service_id = ServiceId::Swap(*swapid);

        self.syncers = self
            .syncers
            .drain()
            .filter_map(
                |(
                    (blockchain, network),
                    Syncer {
                        mut clients,
                        service_id,
                    },
                )| {
                    clients.remove(&client_service_id);
                    if !clients.is_empty() {
                        Some((
                            (blockchain, network),
                            Syncer {
                                clients,
                                service_id,
                            },
                        ))
                    } else {
                        let service_id = ServiceId::Syncer(blockchain, network);
                        info!("Terminating {}", service_id.bright_white_bold());
                        if endpoints
                            .send_to(
                                ServiceBus::Ctl,
                                identity.clone(),
                                service_id.clone(),
                                Request::Terminate,
                            )
                            .is_ok()
                        {
                            None
                        } else {
                            Some((
                                (blockchain, network),
                                Syncer {
                                    clients,
                                    service_id: Some(service_id),
                                },
                            ))
                        }
                    }
                },
            )
            .collect::<HashMap<(Blockchain, Network), Syncer>>();
        Ok(())
    }

    fn consumed_offers_contains(&self, offer: &PublicOffer) -> bool {
        self.consumed_offers.contains_key(offer)
    }

    fn _send_walletd(
        &self,
        endpoints: &mut Endpoints,
        message: request::Request,
    ) -> Result<(), Error> {
        endpoints.send_to(ServiceBus::Ctl, self.identity(), ServiceId::Wallet, message)?;
        Ok(())
    }
    fn node_ids(&self) -> Vec<NodeId> {
        self.node_ids
            .values()
            .into_iter()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    fn _known_swap_id(&self, source: ServiceId) -> Result<SwapId, Error> {
        let swap_id = get_swap_id(&source)?;
        if self.running_swaps.contains(&swap_id) {
            Ok(swap_id)
        } else {
            Err(Error::Farcaster("Unknown swapd".to_string()))
        }
    }
    fn handle_rpc_msg(
        &mut self,
        endpoints: &mut Endpoints,
        source: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        match (&request, &source) {
            (Request::Hello, _) => {
                trace!("Hello farcasterd from {}", source);
                // Ignoring; this is used to set remote identity at ZMQ level
            }

            // 1st protocol message received through peer connection, and last
            // handled by farcasterd, receiving taker commit because we are
            // maker
            (
                Request::Protocol(Msg::TakerCommit(request::TakeCommit {
                    commit: _,
                    public_offer,
                    swap_id,
                })),
                ServiceId::Peer(_),
            ) => {
                // public offer gets removed on LaunchSwap
                if !self.public_offers.contains(&public_offer) {
                    warn!(
                        "Unknown (or already taken) offer {}, you are not the maker of that offer (or you already had a taker for it), ignoring it",
                        &public_offer
                    );
                } else {
                    trace!(
                        "Offer {} is known, you created it previously, engaging walletd to initiate swap with taker",
                        &public_offer
                    );
                    if let Some(arb_addr) = self.arb_addrs.remove(&public_offer.id()) {
                        let btc_addr_req =
                            Request::BitcoinAddress(BitcoinAddress(*swap_id, arb_addr));
                        endpoints.send_to(
                            ServiceBus::Msg,
                            self.identity(),
                            ServiceId::Wallet,
                            btc_addr_req,
                        )?;
                    } else {
                        error!("missing arb_addr")
                    }
                    if let Some(acc_addr) = self.acc_addrs.remove(&public_offer.id()) {
                        let xmr_addr_req =
                            Request::MoneroAddress(MoneroAddress(*swap_id, acc_addr));
                        endpoints.send_to(
                            ServiceBus::Msg,
                            self.identity(),
                            ServiceId::Wallet,
                            xmr_addr_req,
                        )?;
                    } else {
                        error!("missing acc_addr")
                    }
                    info!("passing request to walletd from {}", source);
                    self.peerd_ids
                        .insert(public_offer.offer.id(), source.clone());

                    endpoints.send_to(
                        ServiceBus::Msg,
                        self.identity(),
                        ServiceId::Wallet,
                        request,
                    )?;
                }
                return Ok(());
            }
            _ => {
                error!("MSG RPC can be only used for forwarding farcaster protocol messages");
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
        let mut report_to: Vec<(Option<ServiceId>, Request)> = none!();
        match request.clone() {
            Request::Hello => {
                // Ignoring; this is used to set remote identity at ZMQ level
                info!(
                    "Service {} is now {}",
                    source.bright_white_bold(),
                    "connected".bright_green_bold()
                );

                match &source {
                    ServiceId::Farcasterd => {
                        error!(
                            "{}",
                            "Unexpected another farcasterd instance connection".err()
                        );
                    }
                    ServiceId::Database => {
                        self.databased_ready = true;
                    }
                    ServiceId::Wallet => {
                        self.walletd_ready = true;
                        let wallet_token = GetKeys(self.wallet_token.clone());
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            self.identity(),
                            source.clone(),
                            Request::GetKeys(wallet_token),
                        )?;
                    }
                    ServiceId::Peer(connection_id) => {
                        if self.connections.insert(*connection_id) {
                            info!(
                                "Connection {} is registered; total {} connections are known",
                                connection_id.bright_blue_italic(),
                                self.connections.len().bright_blue_bold()
                            );
                            if let Some(swap_service_id) =
                                self.report_peerd_reconnect.remove(connection_id)
                            {
                                debug!("Letting {} know of peer reconnection.", swap_service_id);
                                endpoints.send_to(
                                    ServiceBus::Ctl,
                                    self.identity(),
                                    swap_service_id,
                                    Request::PeerdReconnected(source.clone()),
                                )?;
                            }
                        } else {
                            warn!(
                                "Connection {} was already registered; the service probably was relaunched",
                                connection_id.bright_blue_italic()
                            );
                        }
                    }
                    ServiceId::Swap(swap_id) => {
                        if self.restoring_swap_id.remove(swap_id) {
                            info!("Restoring swap {}", swap_id.bright_blue_italic());
                            self.stats.incr_initiated();
                            endpoints.send_to(
                                ServiceBus::Ctl,
                                ServiceId::Farcasterd,
                                ServiceId::Database,
                                Request::RestoreCheckpoint(*swap_id),
                            )?;
                        }
                        if self.running_swaps.insert(*swap_id) {
                            info!(
                                "Swap {} is registered; total {} swaps are known",
                                swap_id.bright_blue_italic(),
                                self.running_swaps.len().bright_blue_bold()
                            );
                        } else {
                            warn!(
                                "Swap {} was already registered; the service probably was relaunched",
                                swap_id
                            );
                        }
                    }
                    ServiceId::Syncer(blockchain, network)
                        if !self.syncers.service_online(&(*blockchain, *network))
                            && self.spawning_services.contains_key(&source) =>
                    {
                        if let Some(Syncer { service_id, .. }) =
                            self.syncers.get_mut(&(*blockchain, *network))
                        {
                            *service_id = Some(source.clone());
                            info!(
                                "Syncer {} is registered; total {} syncers are known",
                                &source,
                                self.syncers.syncer_services_len().bright_blue_bold()
                            );
                        }
                        for (source, request) in self.pending_sweep_requests.drain() {
                            endpoints.send_to(
                                ServiceBus::Ctl,
                                source,
                                ServiceId::Syncer(*blockchain, *network),
                                request,
                            )?;
                        }
                        let k = &normalize_syncer_services_pair(&source);
                        if let (ServiceId::Syncer(c0, n0), ServiceId::Syncer(c1, n1)) = k {
                            if self.syncers.pair_ready((*c0, *n0), (*c1, *n1)) {
                                let xs = self.pending_swap_init.get(k).cloned();
                                if let Some(xs) = xs {
                                    for (init_swap_req, swap_id) in xs {
                                        if let Ok(()) = endpoints.send_to(
                                            ServiceBus::Ctl,
                                            self.identity(),
                                            ServiceId::Swap(swap_id),
                                            init_swap_req,
                                        ) {
                                            self.pending_swap_init.remove(k);
                                        } else {
                                            error!("Failed to dispatch init swap requests containing swap params");
                                        };
                                    }
                                }
                            } else {
                                warn!("syncer pair not ready")
                            }
                        }
                    }
                    ServiceId::Syncer(..) => {
                        error!(
                            "Syncer {} was already registered; the service probably was relaunched\\
                             externally, or maybe multiple syncers launched?",
                            source
                        );
                    }
                    _ => {
                        // Ignoring the rest of daemon/client types
                    }
                };

                if let Some((swap_params, network)) = self.making_swaps.get(&source) {
                    // Tell swapd swap options and link it with the
                    // connection daemon
                    debug!(
                        "Swapd {} is known: we spawned it to create a swap. \
                             Requesting swapd to be the maker of this swap",
                        source
                    );
                    // notify this swapd about its syncers that are up and
                    // running. if syncer not ready, then swapd will be notified
                    // on the ServiceId::Syncer(blockchain, network) Hello pattern. in
                    // sum, if syncer is up, send msg immediately else wait for
                    // syncer to say hello, and then dispatch msg
                    let init_swap_req = Request::MakeSwap(swap_params.clone());
                    if self.syncers.pair_ready(
                        (Blockchain::Bitcoin, *network),
                        (Blockchain::Monero, *network),
                    ) {
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            self.identity(),
                            source.clone(),
                            init_swap_req,
                        )?;
                    } else {
                        let xs = self
                            .pending_swap_init
                            .entry((
                                ServiceId::Syncer(Blockchain::Bitcoin, *network),
                                ServiceId::Syncer(Blockchain::Monero, *network),
                            ))
                            .or_insert(vec![]);
                        xs.push((init_swap_req, swap_params.swap_id));
                    }
                    self.running_swaps.insert(swap_params.swap_id);
                    self.making_swaps.remove(&source);
                } else if let Some((swap_params, network)) = self.taking_swaps.get(&source) {
                    // Tell swapd swap options and link it with the
                    // connection daemon
                    debug!(
                        "Daemon {} is known: we spawned it to create a swap. \
                             Requesting swapd to be the taker of this swap",
                        source
                    );
                    let init_swap_req = Request::TakeSwap(swap_params.clone());
                    if self.syncers.pair_ready(
                        (Blockchain::Bitcoin, *network),
                        (Blockchain::Monero, *network),
                    ) {
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            self.identity(),
                            source.clone(),
                            init_swap_req,
                        )?;
                    } else {
                        let xs = self
                            .pending_swap_init
                            .entry((
                                ServiceId::Syncer(Blockchain::Bitcoin, *network),
                                ServiceId::Syncer(Blockchain::Monero, *network),
                            ))
                            .or_insert(vec![]);
                        xs.push((init_swap_req, swap_params.swap_id));
                    }
                    self.running_swaps.insert(swap_params.swap_id);
                    self.taking_swaps.remove(&source);
                } else if let Some(enquirer) = self.spawning_services.get(&source) {
                    debug!(
                        "Daemon {} is known: we spawned it to create a new peer \
                         connection by a request from {}",
                        source, enquirer
                    );
                    self.spawning_services.remove(&source);
                }
            }

            Request::SwapOutcome(success) => {
                let swapid = get_swap_id(&source)?;
                if let Some(public_offer) =
                    self.consumed_offers
                        .iter()
                        .find_map(|(public_offer, (o_swap_id, _))| {
                            if *o_swap_id == swapid {
                                Some(public_offer)
                            } else {
                                None
                            }
                        })
                {
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd,
                        ServiceId::Database,
                        Request::SetOfferStatus(OfferStatusPair {
                            offer: public_offer.clone(),
                            status: OfferStatus::Ended(success.clone()),
                        }),
                    )?;
                }
                self.clean_up_after_swap(&swapid, endpoints)?;
                self.stats.incr_outcome(&success);
                match success {
                    Outcome::Buy => {
                        debug!("Success on swap {}", &swapid);
                    }
                    Outcome::Refund => {
                        warn!("Refund on swap {}", &swapid);
                    }
                    Outcome::Punish => {
                        warn!("Punish on swap {}", &swapid);
                    }
                    Outcome::Abort => {
                        warn!("Aborted swap {}", &swapid);
                    }
                }
                self.stats.success_rate();
            }

            Request::LaunchSwap(LaunchSwap {
                local_trade_role,
                public_offer,
                local_params,
                swap_id,
                remote_commit,
                funding_address,
            }) => {
                let offerid = public_offer.offer.id();
                let network = public_offer.offer.network;
                let peerd_id = self.peerd_ids.get(&offerid); // Some for Maker after TakerCommit, None for Taker
                let peer: ServiceId = match local_trade_role {
                    TradeRole::Maker if peerd_id.is_some() => peerd_id.unwrap().clone(),
                    TradeRole::Taker => internet2::addr::NodeAddr::new(
                        NodeId::from(public_offer.node_id),
                        public_offer.peer_address,
                    )
                    .into(),
                    _ => {
                        error!("peerd_id must exist for Maker after TakerCommit msg!");
                        return Ok(());
                    }
                };
                if self.public_offers.remove(&public_offer) {
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd,
                        ServiceId::Database,
                        Request::SetOfferStatus(OfferStatusPair {
                            offer: public_offer.clone(),
                            status: OfferStatus::InProgress,
                        }),
                    )?;
                    syncers_up(
                        ServiceId::Farcasterd,
                        &mut self.spawning_services,
                        &mut self.syncers,
                        Blockchain::Bitcoin,
                        network,
                        swap_id,
                        &self.config,
                    )?;
                    syncers_up(
                        ServiceId::Farcasterd,
                        &mut self.spawning_services,
                        &mut self.syncers,
                        Blockchain::Monero,
                        network,
                        swap_id,
                        &self.config,
                    )?;
                    trace!(
                        "launching swapd with swap_id: {}",
                        swap_id.bright_yellow_bold()
                    );

                    self.consumed_offers
                        .insert(public_offer.clone(), (swap_id, peer.clone()));
                    self.stats.incr_initiated();
                    launch_swapd(
                        self,
                        peer,
                        Some(self.identity()),
                        local_trade_role,
                        public_offer,
                        local_params,
                        swap_id,
                        remote_commit,
                        funding_address,
                    )?;
                } else {
                    let msg = "unknown public_offer".to_string();
                    error!("{}", msg);
                    return Err(Error::Farcaster(msg));
                }
            }

            Request::Keys(Keys(sk, pk)) => {
                debug!("received peerd keys {}", sk.display_secret());
                self.node_secret_key = Some(sk);
                self.node_public_key = Some(pk);
            }

            Request::GetInfo => {
                debug!("farcasterd received GetInfo request");
                self.send_client_ctl(
                    endpoints,
                    source,
                    Request::NodeInfo(NodeInfo {
                        node_ids: self.node_ids(),
                        listens: self.listens.values().into_iter().cloned().collect(),
                        uptime: SystemTime::now()
                            .duration_since(self.started)
                            .unwrap_or_else(|_| Duration::from_secs(0)),
                        since: self
                            .started
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or_else(|_| Duration::from_secs(0))
                            .as_secs(),
                        peers: self.connections.iter().cloned().collect(),
                        swaps: self.running_swaps.iter().cloned().collect(),
                        offers: self.public_offers.iter().cloned().collect(),
                    }),
                )?;
            }

            Request::ListPeers => {
                endpoints.send_to(
                    ServiceBus::Ctl,
                    ServiceId::Farcasterd, // source
                    source,                // destination
                    Request::PeerList(self.connections.iter().cloned().collect()),
                )?;
            }

            Request::ListSwaps => {
                endpoints.send_to(
                    ServiceBus::Ctl,
                    ServiceId::Farcasterd, // source
                    source,                // destination
                    Request::SwapList(self.running_swaps.iter().cloned().collect()),
                )?;
            }

            Request::ListOffers(offer_status_selector) => {
                match offer_status_selector {
                    OfferStatusSelector::Open => {
                        let pub_offers = self
                            .public_offers
                            .iter()
                            .filter(|k| !self.consumed_offers_contains(k))
                            .map(|offer| OfferInfo {
                                offer: offer.to_string(),
                                details: offer.clone(),
                            })
                            .collect();
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            ServiceId::Farcasterd, // source
                            source,                // destination
                            Request::OfferList(pub_offers),
                        )?;
                    }
                    OfferStatusSelector::InProgress => {
                        let pub_offers = self
                            .consumed_offers
                            .keys()
                            .cloned()
                            .map(|offer| OfferInfo {
                                offer: offer.to_string(),
                                details: offer,
                            })
                            .collect();
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            ServiceId::Farcasterd,
                            source,
                            Request::OfferList(pub_offers),
                        )?;
                    }
                    _ => {
                        endpoints.send_to(ServiceBus::Ctl, source, ServiceId::Database, request)?;
                    }
                };
            }

            Request::RevokeOffer(public_offer) => {
                debug!("attempting to revoke {}", public_offer);
                if self.public_offers.remove(&public_offer) {
                    info!("Revoked offer {}", public_offer);
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd,
                        source,
                        Request::String("Successfully revoked offer.".to_string()),
                    )?;
                } else {
                    error!("failed to revoke {}", public_offer);
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd,
                        source,
                        Request::Failure(Failure {
                            code: FailureCode::Unknown,
                            info: "Coulod not find to be revoked offer.".to_string(),
                        }),
                    )?;
                }
            }

            Request::ListListens => {
                let listen_url: List<String> = List::from_iter(
                    self.listens
                        .clone()
                        .values()
                        .map(|listen| listen.to_string()),
                );
                endpoints.send_to(
                    ServiceBus::Ctl,
                    ServiceId::Farcasterd, // source
                    source,                // destination
                    Request::ListenList(listen_url),
                )?;
            }

            Request::CheckpointList(checkpointed_pub_offers) => {
                self.checkpointed_pub_offers = checkpointed_pub_offers.clone();
                endpoints.send_to(
                    ServiceBus::Ctl,
                    ServiceId::Farcasterd,
                    source,
                    Request::CheckpointList(checkpointed_pub_offers),
                )?;
            }

            Request::RestoreCheckpoint(swap_id) => {
                if let Err(err) = self.services_ready() {
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd,
                        source.clone(),
                        Request::Failure(Failure {
                            code: FailureCode::Unknown,
                            info: err.to_string(),
                        }),
                    )?
                }
                // check if wallet is running
                if endpoints
                    .send_to(
                        ServiceBus::Msg,
                        ServiceId::Farcasterd,
                        ServiceId::Wallet,
                        Request::Hello,
                    )
                    .is_err()
                {
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd,
                        source,
                        Request::Failure(Failure {
                            code: FailureCode::Unknown,
                            info: "Cannot restore a swap when walletd is not running".to_string(),
                        }),
                    )?;
                    return Ok(());
                }

                // check if swapd is not running
                if endpoints
                    .send_to(
                        ServiceBus::Msg,
                        ServiceId::Farcasterd,
                        ServiceId::Swap(swap_id),
                        Request::Hello,
                    )
                    .is_ok()
                {
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd,
                        source,
                        Request::Failure(Failure {
                            code: FailureCode::Unknown,
                            info: "Cannot restore a checkpoint into a running swap.".to_string(),
                        }),
                    )?;
                    return Ok(());
                }

                let CheckpointEntry {
                    public_offer,
                    trade_role,
                    ..
                } = match self
                    .checkpointed_pub_offers
                    .iter()
                    .find(|entry| entry.swap_id == swap_id)
                {
                    Some(ce) => ce,
                    None => {
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            ServiceId::Farcasterd,
                            source,
                            Request::Failure(Failure {
                                code: FailureCode::Unknown,
                                info: "No checkpoint found with given swap id, aborting restore."
                                    .to_string(),
                            }),
                        )?;
                        return Ok(());
                    }
                };
                self.restoring_swap_id.insert(swap_id);
                syncers_up(
                    ServiceId::Farcasterd,
                    &mut self.spawning_services,
                    &mut self.syncers,
                    Blockchain::Bitcoin,
                    public_offer.offer.network,
                    swap_id,
                    &self.config,
                )?;
                syncers_up(
                    ServiceId::Farcasterd,
                    &mut self.spawning_services,
                    &mut self.syncers,
                    Blockchain::Monero,
                    public_offer.offer.network,
                    swap_id,
                    &self.config,
                )?;

                let _child = launch(
                    "swapd",
                    &[
                        swap_id.to_hex(),
                        public_offer.to_string(),
                        trade_role.to_string(),
                    ],
                )?;

                endpoints.send_to(
                    ServiceBus::Ctl,
                    ServiceId::Farcasterd,
                    source,
                    Request::String("Restoring checkpoint.".to_string()),
                )?;
            }

            Request::MakeOffer(request::ProtoPublicOffer {
                offer,
                public_addr,
                bind_addr,
                arbitrating_addr,
                accordant_addr,
            }) => {
                let res = self.services_ready().and_then(|_| {
                    let (peer_secret_key, peer_public_key) = self.peer_keys_ready()?;
                    let address_bound = self.listens.iter().any(|(_, a)| a == &bind_addr);
                    if !address_bound {
                        // if address not bound, bind first
                        info!(
                            "{} for incoming peer connections on {}",
                            "Starting listener".bright_blue_bold(),
                            bind_addr.bright_blue_bold()
                        );
                        let node_id = NodeId::from(peer_public_key);
                        self.listen(NodeAddr::new(node_id, bind_addr), peer_secret_key)
                            .and_then(|_| {
                                self.listens.insert(offer.id(), bind_addr);
                                self.node_ids.insert(offer.id(), node_id);
                                Ok(())
                            })?;
                    } else {
                        // no need for the keys, because peerd already knows them
                        self.listens.insert(offer.id(), bind_addr);
                        self.node_ids
                            .insert(offer.id(), NodeId::from(peer_public_key));
                        let msg = format!("Already listening on {}", &bind_addr);
                        debug!("{}", &msg);
                    }
                    info!(
                        "Connection daemon {} for incoming peer connections on {}",
                        "listens".bright_green_bold(),
                        bind_addr
                    );
                    let node_id = self.node_ids.get(&offer.id()).cloned().unwrap();
                    let public_offer = offer.to_public_v1(node_id.public_key(), public_addr);
                    let pub_offer_id = public_offer.id();
                    let serialized_offer = public_offer.to_string();
                    if !self.public_offers.insert(public_offer.clone()) {
                        let msg = s!("This Public offer was previously registered");
                        error!("{}", msg.err());
                        return Err(Error::Other(msg));
                    }
                    let msg = s!("Public offer registered, please share with taker.");
                    info!(
                        "{}: {:#}",
                        "Public offer registered.".bright_green_bold(),
                        pub_offer_id.bright_yellow_bold()
                    );
                    self.arb_addrs.insert(pub_offer_id, arbitrating_addr);
                    self.acc_addrs.insert(pub_offer_id, accordant_addr);
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd,
                        ServiceId::Database,
                        Request::SetOfferStatus(OfferStatusPair {
                            offer: public_offer.clone(),
                            status: OfferStatus::Open,
                        }),
                    )?;
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd, // source
                        source.clone(),        // destination
                        Request::MadeOffer(MadeOffer {
                            message: msg,
                            offer_info: OfferInfo {
                                offer: serialized_offer,
                                details: public_offer,
                            },
                        }),
                    )?;
                    Ok(())
                });
                if let Err(err) = res {
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd,
                        source.clone(),
                        Request::Failure(Failure {
                            code: FailureCode::Unknown,
                            info: err.to_string(),
                        }),
                    )?
                }
            }

            Request::TakeOffer(request::PubOffer {
                public_offer,
                external_address,
                internal_address,
            }) => {
                if self.public_offers.contains(&public_offer)
                    || self.consumed_offers_contains(&public_offer)
                {
                    let msg = format!(
                        "{} already exists or was already taken, ignoring request",
                        &public_offer.to_string()
                    );
                    warn!("{}", msg.err());
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd, // source
                        source.clone(),        // destination
                        Request::Failure(Failure {
                            code: FailureCode::Unknown,
                            info: msg,
                        }),
                    )?;
                    return Ok(());
                }
                let PublicOffer {
                    version: _,
                    offer: _,
                    node_id,      // bitcoin::Pubkey
                    peer_address, // InetSocketAddr
                } = public_offer;

                let peer = internet2::addr::NodeAddr {
                    id: NodeId::from(node_id), // checked above
                    addr: peer_address,
                };

                let res = self.services_ready().and_then(|_| {
                    let (peer_secret_key, _) = self.peer_keys_ready()?;
                    // Connect
                    if self.connections.contains(&peer) {
                        let msg = format!(
                            "Already connected to remote peer {}",
                            peer.bright_blue_italic()
                        );
                        warn!("{}", &msg);
                    } else {
                        debug!(
                            "{} to remote peer {}",
                            "Connecting".bright_blue_bold(),
                            peer.bright_blue_italic()
                        );
                        self.connect_peer(source.clone(), &peer, peer_secret_key)?;
                    }
                    let offer_registered = "Public offer registered".to_string();
                    // not yet in the set
                    self.public_offers.insert(public_offer.clone());
                    info!(
                        "{}: {:#}",
                        offer_registered.bright_green_bold(),
                        &public_offer.id().bright_yellow_bold()
                    );

                    let request = Request::TakeOffer(PubOffer {
                        public_offer: public_offer.clone(),
                        external_address,
                        internal_address,
                    });
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        self.identity(),
                        ServiceId::Wallet,
                        request,
                    )?;
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd, // source
                        source.clone(),        // destination
                        Request::TookOffer(TookOffer {
                            offerid: public_offer.id(),
                            message: offer_registered,
                        }),
                    )?;
                    Ok(())
                });
                if let Err(err) = res {
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd,
                        source.clone(),
                        Request::Failure(Failure {
                            code: FailureCode::Unknown,
                            info: err.to_string(),
                        }),
                    )?
                }
            }

            // Add progress in queues and forward to subscribed clients
            Request::Progress(..) | Request::Success(..) | Request::Failure(..) => {
                if !self.progress.contains_key(&source) {
                    self.progress.insert(source.clone(), none!());
                };
                let queue = self.progress.get_mut(&source).expect("checked/added above");
                queue.push_back(request.clone());
                // forward the request to each subscribed clients
                self.notify_subscribed_clients(endpoints, &source, &request);
            }

            // Returns a unique response that contains the complete progress queue
            Request::ReadProgress(swapid) => {
                if let Some(queue) = self.progress.get_mut(&ServiceId::Swap(swapid)) {
                    let mut swap_progress = SwapProgress { progress: vec![] };
                    for req in queue.iter() {
                        match req {
                            Request::Progress(request::Progress::Message(m)) => {
                                swap_progress
                                    .progress
                                    .push(ProgressEvent::Message(m.clone()));
                            }
                            Request::Progress(request::Progress::StateTransition(t)) => {
                                swap_progress
                                    .progress
                                    .push(ProgressEvent::StateTransition(t.clone()));
                            }
                            Request::Success(s) => {
                                swap_progress
                                    .progress
                                    .push(ProgressEvent::Success(s.clone()));
                            }
                            Request::Failure(f) => {
                                swap_progress
                                    .progress
                                    .push(ProgressEvent::Failure(f.clone()));
                            }
                            _ => unreachable!("not handled here"),
                        };
                    }
                    report_to.push((Some(source.clone()), Request::SwapProgress(swap_progress)));
                } else {
                    let info = if self.making_swaps.contains_key(&ServiceId::Swap(swapid))
                        || self.taking_swaps.contains_key(&ServiceId::Swap(swapid))
                    {
                        s!("No progress made yet on this swap")
                    } else {
                        s!("Unknown swapd")
                    };
                    report_to.push((
                        Some(source.clone()),
                        Request::Failure(Failure {
                            code: FailureCode::Unknown,
                            info,
                        }),
                    ));
                }
            }

            // Add the request's source to the subscription list for later progress notifications
            // and send all notifications already in the queue
            Request::SubscribeProgress(swapid) => {
                let service = ServiceId::Swap(swapid);
                // if the swap is known either in taking, making or progress, attach the client
                // otherwise terminate
                if self.making_swaps.contains_key(&service)
                    || self.taking_swaps.contains_key(&service)
                    || self.progress.contains_key(&service)
                {
                    if let Some(subscribed) = self.progress_subscriptions.get_mut(&service) {
                        // ret true if not in the set, false otherwise. Double subscribe is not a
                        // problem as we manage the list in a set.
                        let _ = subscribed.insert(source.clone());
                    } else {
                        let mut subscribed = HashSet::new();
                        subscribed.insert(source.clone());
                        // None is returned, the key was not set as checked before
                        let _ = self
                            .progress_subscriptions
                            .insert(service.clone(), subscribed);
                    }
                    trace!(
                        "{} has been added to {} progress subscription",
                        source.clone(),
                        swapid
                    );
                    // send all queued notification to the source to catch up
                    if let Some(queue) = self.progress.get_mut(&service) {
                        for req in queue.iter() {
                            report_to.push((Some(source.clone()), req.clone()));
                        }
                    }
                } else {
                    // no swap service exists, terminate
                    report_to.push((
                        Some(source.clone()),
                        Request::Failure(Failure {
                            code: FailureCode::Unknown,
                            info: "Unknown swapd".to_string(),
                        }),
                    ));
                }
            }

            // Remove the request's source from the subscription list of notifications
            Request::UnsubscribeProgress(swapid) => {
                let service = ServiceId::Swap(swapid);
                if let Some(subscribed) = self.progress_subscriptions.get_mut(&service) {
                    // we don't care if the source was not in the set
                    let _ = subscribed.remove(&source);
                    trace!(
                        "{} has been removed from {} progress subscription",
                        source.clone(),
                        swapid
                    );
                    if subscribed.is_empty() {
                        // we drop the empty set located at the swap index
                        let _ = self.progress_subscriptions.remove(&service);
                    }
                }
                // if no swap service exists no subscription need to be removed
            }

            Request::FundingInfo(info) => match info {
                FundingInfo::Bitcoin(BitcoinFundingInfo {
                    swap_id,
                    address,
                    amount,
                }) => {
                    self.stats.incr_awaiting_funding(&Blockchain::Bitcoin);
                    let network = address.network.into();
                    if let Some(auto_fund_config) = self.config.get_auto_funding_config(network) {
                        info!(
                            "{} | Attempting to auto-fund Bitcoin",
                            swap_id.bright_blue_italic()
                        );
                        debug!(
                            "{} | Auto funding config: {:#?}",
                            swap_id.bright_blue_italic(),
                            auto_fund_config
                        );

                        use bitcoincore_rpc::{Auth, Client, Error, RpcApi};
                        use std::path::PathBuf;

                        let host = auto_fund_config.bitcoin_rpc;
                        let bitcoin_rpc = match auto_fund_config.bitcoin_cookie_path {
                            Some(cookie) => {
                                let path = PathBuf::from_str(&shellexpand::tilde(&cookie)).unwrap();
                                debug!("{} | bitcoin-rpc connecting with cookie auth",
                                       swap_id.bright_blue_italic());
                                Client::new(&host, Auth::CookieFile(path))
                            }
                            None => {
                                match (auto_fund_config.bitcoin_rpc_user, auto_fund_config.bitcoin_rpc_pass) {
                                    (Some(rpc_user), Some(rpc_pass)) => {
                                        debug!("{} | bitcoin-rpc connecting with userpass auth",
                                               swap_id.bright_blue_italic());
                                        Client::new(&host, Auth::UserPass(rpc_user, rpc_pass))
                                    }
                                    _ => {
                                        error!(
                                            "{} | Couldn't instantiate Bitcoin RPC - provide either `bitcoin_cookie_path` or `bitcoin_rpc_user` AND `bitcoin_rpc_pass` configuration parameters",
                                            swap_id.bright_blue_italic()
                                        );

                                        Err(Error::InvalidCookieFile)}
                                }
                            }
                        }.unwrap();

                        match bitcoin_rpc
                            .send_to_address(&address, amount, None, None, None, None, None, None)
                        {
                            Ok(txid) => {
                                info!(
                                    "{} | Auto-funded Bitcoin with txid: {}",
                                    swap_id.bright_blue_italic(),
                                    txid
                                );
                                self.funding_btc.insert(swap_id, (address, amount, true));
                            }
                            Err(err) => {
                                warn!("{}", err);
                                error!(
                                    "{} | Auto-funding Bitcoin transaction failed, pushing to cli, use `swap-cli needs-funding Bitcoin` to retrieve address and amount",
                                    swap_id.bright_blue_italic()
                                );
                                self.funding_btc.insert(swap_id, (address, amount, false));
                            }
                        }
                    } else {
                        self.funding_btc.insert(swap_id, (address, amount, false));
                    }
                }
                FundingInfo::Monero(MoneroFundingInfo {
                    swap_id,
                    address,
                    amount,
                }) => {
                    self.stats.incr_awaiting_funding(&Blockchain::Monero);
                    let network = address.network.into();
                    if let Some(auto_fund_config) = self.config.get_auto_funding_config(network) {
                        info!(
                            "{} | Attempting to auto-fund Monero",
                            swap_id.bright_blue_italic()
                        );
                        debug!(
                            "{} | Auto funding config: {:#?}",
                            swap_id.bright_blue_italic(),
                            auto_fund_config
                        );
                        use tokio::runtime::Builder;
                        let rt = Builder::new_multi_thread()
                            .worker_threads(1)
                            .enable_all()
                            .build()
                            .unwrap();
                        rt.block_on(async {
                            let host = auto_fund_config.monero_rpc_wallet;
                            let wallet_client =
                                monero_rpc::RpcClient::new(host);
                            let wallet = wallet_client.wallet();
                            let options = monero_rpc::TransferOptions::default();
                            for retries in (0..10).rev() {
                                match wallet
                                    .transfer(
                                        [(address, amount)].iter().cloned().collect(),
                                        monero_rpc::TransferPriority::Default,
                                        options.clone(),
                                    )
                                    .await
                                {
                                    Ok(tx) => {
                                        info!(
                                            "{} | Auto-funded Monero with txid: {}",
                                            &swap_id.bright_blue_italic(),
                                            tx.tx_hash.to_string()
                                        );
                                        self.funding_xmr.insert(swap_id, (address, amount, true));
                                        break;
                                    }
                                    Err(err) => {
                                        if (err.to_string().contains("not enough") && err.to_string().contains("money")) || retries == 0 {
                                            error!("{} | Auto-funding Monero transaction failed with {}, pushing to cli, use `swap-cli needs-funding Monero` to retrieve address and amount", &swap_id.bright_blue_italic(), err);
                                            self.funding_xmr.insert(swap_id, (address, amount, false));
                                            break;
                                        } else {
                                            warn!("{} | Auto-funding Monero transaction failed with {}, retrying, {} retries left", &swap_id.bright_blue_italic(), err, retries);
                                        }
                                    }
                                }
                            }
                        });
                    } else {
                        self.funding_xmr.insert(swap_id, (address, amount, false));
                    }
                }
            },

            Request::FundingCompleted(blockchain) => {
                let swapid = get_swap_id(&source)?;
                if match blockchain {
                    Blockchain::Bitcoin => {
                        self.funding_btc.remove(&get_swap_id(&source)?).is_some()
                    }
                    Blockchain::Monero => self.funding_xmr.remove(&get_swap_id(&source)?).is_some(),
                } {
                    self.stats.incr_funded(&blockchain);
                    info!(
                        "{} | Your {} funding completed",
                        swapid.bright_blue_italic(),
                        blockchain.bright_green_bold()
                    );
                }
            }

            Request::FundingCanceled(blockchain) => {
                let swapid = get_swap_id(&source)?;
                match blockchain {
                    Blockchain::Bitcoin => {
                        if self.funding_btc.remove(&get_swap_id(&source)?).is_some() {
                            self.stats.incr_funding_bitcoin_canceled();
                            info!(
                                "{} | Your {} funding was canceled",
                                swapid.bright_blue_italic(),
                                blockchain.bright_green_bold()
                            );
                        }
                    }
                    Blockchain::Monero => {
                        if self.funding_xmr.remove(&get_swap_id(&source)?).is_some() {
                            self.stats.incr_funding_monero_canceled();
                            info!(
                                "{} | Your {} funding was canceled",
                                swapid.bright_blue_italic(),
                                blockchain.bright_green_bold()
                            );
                        }
                    }
                };
            }

            Request::NeedsFunding(Blockchain::Monero) => {
                let len = self.funding_xmr.len();
                let res = self
                    .funding_xmr
                    .iter()
                    .filter(|(_, (_, _, autofund))| !*autofund)
                    .enumerate()
                    .map(|(i, (swap_id, (address, amount, _)))| {
                        let mut res = format!(
                            "{}",
                            MoneroFundingInfo {
                                swap_id: *swap_id,
                                amount: *amount,
                                address: *address,
                            }
                        );
                        if i < len - 1 {
                            res.push('\n');
                        }
                        res
                    })
                    .collect();
                endpoints.send_to(
                    ServiceBus::Ctl,
                    self.identity(),
                    source,
                    Request::String(res),
                )?;
            }
            Request::NeedsFunding(Blockchain::Bitcoin) => {
                let len = self.funding_btc.len();
                let res = self
                    .funding_btc
                    .iter()
                    .filter(|(_, (_, _, autofund))| !*autofund)
                    .enumerate()
                    .map(|(i, (swap_id, (address, amount, _)))| {
                        let mut res = format!(
                            "{}",
                            BitcoinFundingInfo {
                                swap_id: *swap_id,
                                amount: *amount,
                                address: address.clone(),
                            }
                        );
                        if i < len - 1 {
                            res.push('\n');
                        }
                        res
                    })
                    .collect();
                endpoints.send_to(
                    ServiceBus::Ctl,
                    self.identity(),
                    source,
                    Request::String(res),
                )?;
            }

            Request::SweepMoneroAddress(sweep_xmr_address) => {
                let blockchain = Blockchain::Monero;
                let network = sweep_xmr_address.destination_address.network.into();
                // check if a monero syncer is up
                let id = TaskId(self.syncer_task_counter);
                let request = Request::SyncerTask(Task::SweepAddress(SweepAddress {
                    id,
                    retry: false,
                    lifetime: u64::MAX,
                    addendum: SweepAddressAddendum::Monero(sweep_xmr_address),
                    from_height: None,
                }));
                self.syncer_task_counter += 1;
                self.syncer_tasks.insert(id, source.clone());

                let k = (blockchain, network);
                let s = ServiceId::Syncer(blockchain, network);
                if !self.syncers.service_online(&k) && !self.spawning_services.contains_key(&s) {
                    let mut args = vec![
                        "--blockchain".to_string(),
                        blockchain.to_string(),
                        "--network".to_string(),
                        network.to_string(),
                    ];
                    args.append(
                        &mut syncer_servers_args(&self.config, blockchain, network).or(
                            syncer_servers_args(&self.config, blockchain, Network::Local),
                        )?,
                    );
                    info!("launching syncer with: {:?}", args);
                    launch("syncerd", args)?;
                    self.spawning_services.insert(s, ServiceId::Farcasterd);
                    if let Some(syncer) = self.syncers.get_mut(&k) {
                        syncer.clients.insert(source.clone());
                    } else {
                        self.syncers.insert(
                            k,
                            Syncer {
                                service_id: None,
                                clients: set![source.clone()],
                            },
                        );
                    }
                    self.pending_sweep_requests.insert(source, request);
                } else {
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd,
                        ServiceId::Syncer(blockchain, network),
                        request,
                    )?;
                }
            }

            Request::SweepBitcoinAddress(sweep_bitcoin_address) => {
                let blockchain = Blockchain::Bitcoin;
                let network = sweep_bitcoin_address.source_address.network.into();
                // check if a bitcoin syncer is up
                let id = TaskId(self.syncer_task_counter);
                let request = Request::SyncerTask(Task::SweepAddress(SweepAddress {
                    id,
                    retry: false,
                    lifetime: u64::MAX,
                    addendum: SweepAddressAddendum::Bitcoin(sweep_bitcoin_address),
                    from_height: None,
                }));
                self.syncer_task_counter += 1;
                self.syncer_tasks.insert(id, source.clone());

                let k = (blockchain, network);
                let s = ServiceId::Syncer(blockchain, network);
                if !self.syncers.service_online(&k) && !self.spawning_services.contains_key(&s) {
                    let mut args = vec![
                        "--blockchain".to_string(),
                        blockchain.to_string(),
                        "--network".to_string(),
                        network.to_string(),
                    ];
                    args.append(&mut syncer_servers_args(&self.config, blockchain, network)?);
                    info!("launching syncer with: {:?}", args);
                    launch("syncerd", args)?;
                    self.spawning_services.insert(s, ServiceId::Farcasterd);
                    if let Some(syncer) = self.syncers.get_mut(&k) {
                        syncer.clients.insert(source.clone());
                    } else {
                        self.syncers.insert(
                            k,
                            Syncer {
                                clients: set![source.clone()],
                                service_id: None,
                            },
                        );
                    }
                    self.pending_sweep_requests.insert(source, request);
                } else {
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        source,
                        ServiceId::Syncer(blockchain, network),
                        request,
                    )?;
                }
            }

            Request::SyncerEvent(Event::SweepSuccess(success))
                if self.syncer_tasks.contains_key(&success.id) =>
            {
                let client_service_id = self
                    .syncer_tasks
                    .remove(&success.id)
                    .expect("checked by guard");
                if let Some(Some(txid)) = success
                    .txids
                    .clone()
                    .pop()
                    .map(|txid| bitcoin::Txid::from_slice(&txid).ok())
                {
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd,
                        client_service_id.clone(),
                        Request::String(format!(
                            "Successfully sweeped address. Transaction Id: {}.",
                            txid.to_hex()
                        )),
                    )?;
                } else {
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd,
                        client_service_id.clone(),
                        Request::String("Nothing to sweep.".to_string()),
                    )?;
                }

                self.syncers = self
                    .syncers
                    .drain()
                    .filter_map(|((blockchain, network), mut xs)| {
                        xs.clients.remove(&client_service_id);
                        if !xs.clients.is_empty() {
                            Some(((blockchain, network), xs))
                        } else {
                            let service_id = ServiceId::Syncer(blockchain, network);
                            info!("Terminating {}", service_id.bright_white_bold());
                            if endpoints
                                .send_to(
                                    ServiceBus::Ctl,
                                    ServiceId::Farcasterd,
                                    service_id,
                                    Request::Terminate,
                                )
                                .is_ok()
                            {
                                None
                            } else {
                                Some(((blockchain, network), xs))
                            }
                        }
                    })
                    .collect::<HashMap<(Blockchain, Network), Syncer>>();
            }

            Request::PeerdTerminated => {
                if let ServiceId::Peer(addr) = source {
                    if self.connections.remove(&addr) {
                        debug!(
                            "removed connection {} from farcasterd registered connections",
                            addr
                        );

                        // log a message if a swap running over this connection
                        // is not completed, and thus present in consumed_offers
                        let peerd_id = ServiceId::Peer(addr);
                        if self
                            .consumed_offers
                            .iter()
                            .any(|(_, (_, service_id))| *service_id == peerd_id)
                        {
                            info!("a swap is still running over the terminated peer {}, the counterparty will attempt to reconnect.", addr);
                        }
                    }
                }
            }

            Request::PeerdUnreachable(ServiceId::Peer(addr)) => {
                if self.connections.contains(&addr) {
                    warn!(
                        "Peerd {} was reported to be unreachable, attempting to
                        terminate to kick-off re-connect procedure, if we are
                        taker and the swap is still running.",
                        addr
                    );
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd,
                        ServiceId::Peer(addr),
                        Request::Terminate,
                    )?;
                }
                self.report_peerd_reconnect.insert(addr, source);
            }

            req => {
                error!("Ignoring unsupported request: {}", req.err());
            }
        }

        for (i, (respond_to, resp)) in report_to.clone().into_iter().enumerate() {
            if let Some(respond_to) = respond_to {
                // do not respond to self
                if respond_to == self.identity() {
                    continue;
                }
                trace!(
                    "(#{}) Respond to {}: {}",
                    i,
                    respond_to.bright_yellow_bold(),
                    resp.bright_blue_bold(),
                );
                endpoints.send_to(ServiceBus::Ctl, self.identity(), respond_to, resp)?;
            }
        }
        trace!("Processed all cli notifications");

        Ok(())
    }

    fn listen(&mut self, addr: NodeAddr, sk: SecretKey) -> Result<(), Error> {
        let address = addr.addr.address();
        let port = addr.addr.port().ok_or(Error::Farcaster(
            "listen requires the port to listen on".to_string(),
        ))?;

        debug!("Instantiating peerd...");
        let child = launch(
            "peerd",
            &[
                "--listen",
                &format!("{}", address),
                "--port",
                &port.to_string(),
                "--peer-secret-key",
                &format!("{}", sk.display_secret()),
                "--token",
                &self.wallet_token.clone().to_string(),
            ],
        );

        // in case it can't connect wait for it to crash
        std::thread::sleep(Duration::from_secs_f32(0.5));

        // status is Some if peerd returns because it crashed
        let (child, status) = child.and_then(|mut c| c.try_wait().map(|s| (c, s)))?;

        if status.is_some() {
            return Err(Error::Peer(internet2::presentation::Error::InvalidEndpoint));
        }

        debug!("New instance of peerd launched with PID {}", child.id());
        Ok(())
    }

    fn connect_peer(
        &mut self,
        source: ServiceId,
        node_addr: &NodeAddr,
        sk: SecretKey,
    ) -> Result<(), Error> {
        debug!("Instantiating peerd...");
        if self.connections.contains(node_addr) {
            return Err(Error::Other(format!(
                "Already connected to peer {}",
                node_addr
            )));
        }

        // Start peerd
        let child = launch(
            "peerd",
            &[
                "--connect",
                &node_addr.to_string(),
                "--peer-secret-key",
                &format!("{}", sk.display_secret()),
                "--token",
                &self.wallet_token.clone().to_string(),
            ],
        );

        // in case it can't connect wait for it to crash
        std::thread::sleep(Duration::from_secs_f32(0.5));

        // status is Some if peerd returns because it crashed
        let (child, status) = child.and_then(|mut c| c.try_wait().map(|s| (c, s)))?;

        if status.is_some() {
            return Err(Error::Peer(internet2::presentation::Error::InvalidEndpoint));
        }

        debug!("New instance of peerd launched with PID {}", child.id());

        self.spawning_services
            .insert(ServiceId::Peer(*node_addr), source);
        debug!("Awaiting for peerd to connect...");

        Ok(())
    }

    /// Notify(forward to) the subscribed clients still online with the given request
    fn notify_subscribed_clients(
        &mut self,
        endpoints: &mut Endpoints,
        source: &ServiceId,
        request: &Request,
    ) {
        // if subs exists for the source (swapid), forward the request to every subs
        if let Some(subs) = self.progress_subscriptions.get_mut(source) {
            // if the sub is no longer reachable, i.e. the process terminated without calling
            // unsub, remove it from sub list
            subs.retain(|sub| {
                endpoints
                    .send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd,
                        sub.clone(),
                        request.clone(),
                    )
                    .is_ok()
            });
        }
    }
}

fn syncers_up(
    source: ServiceId,
    spawning_services: &mut HashMap<ServiceId, ServiceId>,
    syncers: &mut Syncers,
    blockchain: Blockchain,
    network: Network,
    swap_id: SwapId,
    config: &Config,
) -> Result<(), Error> {
    let k = (blockchain, network);
    let s = ServiceId::Syncer(blockchain, network);
    if !syncers.service_online(&k) && !spawning_services.contains_key(&s) {
        let mut args = vec![
            "--blockchain".to_string(),
            blockchain.to_string(),
            "--network".to_string(),
            network.to_string(),
        ];
        args.append(&mut syncer_servers_args(config, blockchain, network)?);
        info!("launching syncer with: {:?}", args);
        launch("syncerd", args)?;
        syncers.insert(
            k,
            Syncer {
                clients: none!(),
                service_id: None,
            },
        );
        spawning_services.insert(s, source);
    }
    if let Some(xs) = syncers.get_mut(&k) {
        xs.clients.insert(ServiceId::Swap(swap_id));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn launch_swapd(
    runtime: &mut Runtime,
    peerd: ServiceId,
    report_to: Option<ServiceId>,
    local_trade_role: TradeRole,
    public_offer: PublicOffer,
    local_params: Params,
    swap_id: SwapId,
    remote_commit: Option<Commit>,
    funding_address: Option<bitcoin::Address>,
) -> Result<String, Error> {
    debug!("Instantiating swapd...");
    let child = launch(
        "swapd",
        &[
            swap_id.to_hex(),
            public_offer.to_string(),
            local_trade_role.to_string(),
        ],
    )?;
    let msg = format!("New instance of swapd launched with PID {}", child.id());
    debug!("{}", msg);

    let list = match local_trade_role {
        TradeRole::Taker => &mut runtime.taking_swaps,
        TradeRole::Maker => &mut runtime.making_swaps,
    };
    list.insert(
        ServiceId::Swap(swap_id),
        (
            request::InitSwap {
                peerd,
                report_to,
                local_params,
                swap_id,
                remote_commit,
                funding_address,
            },
            public_offer.offer.network,
        ),
    );

    debug!("Awaiting for swapd to connect...");

    Ok(msg)
}

/// Return the list of needed arguments for a syncer given a config and a network.
/// This function only register the minimal set of URLs needed for the blockchain to work.
fn syncer_servers_args(
    config: &Config,
    blockchain: Blockchain,
    net: Network,
) -> Result<Vec<String>, Error> {
    match config.get_syncer_servers(net) {
        Some(servers) => match blockchain {
            Blockchain::Bitcoin => Ok(vec![
                "--electrum-server".to_string(),
                servers.electrum_server,
            ]),
            Blockchain::Monero => {
                let mut args: Vec<String> = vec![
                    "--monero-daemon".to_string(),
                    servers.monero_daemon,
                    "--monero-rpc-wallet".to_string(),
                    servers.monero_rpc_wallet,
                ];
                args.extend(
                    servers
                        .monero_lws
                        .map_or(vec![], |v| vec!["--monero-lws".to_string(), v]),
                );
                args.extend(
                    servers
                        .monero_wallet_dir
                        .map_or(vec![], |v| vec!["--monero-wallet-dir-path".to_string(), v]),
                );
                Ok(args)
            }
        },
        None => Err(SyncerError::InvalidConfig.into()),
    }
}

pub fn launch(
    name: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> io::Result<process::Child> {
    let app = Opts::command();
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

    // Forwarded shared options from farcasterd to launched microservices
    // Cannot use value_of directly because of default values
    let matches = app.get_matches();

    if let Some(d) = &matches.value_of("data-dir") {
        cmd.args(&["-d", d]);
    }

    if let Some(m) = &matches.value_of("msg-socket") {
        cmd.args(&["-m", m]);
    }

    if let Some(x) = &matches.value_of("ctl-socket") {
        cmd.args(&["-x", x]);
    }

    // Forward tor proxy argument
    let parsed = Opts::parse();
    info!("tor opts: {:?}", parsed.shared.tor_proxy);
    if let Some(t) = &matches.value_of("tor-proxy") {
        cmd.args(&["-T", *t]);
    }

    // Given specialized args in launch
    cmd.args(args);

    debug!("Executing `{:?}`", cmd);
    cmd.spawn().map_err(|err| {
        error!("Error launching {}: {}", name, err);
        err
    })
}
type Syncers = HashMap<(Blockchain, Network), Syncer>;

trait SyncersT {
    fn syncer_services_len(&self) -> usize;
    fn service_online(&self, key: &(Blockchain, Network)) -> bool;
    fn pair_ready(&self, coin0: (Blockchain, Network), coin1: (Blockchain, Network)) -> bool;
}

impl SyncersT for Syncers {
    fn syncer_services_len(&self) -> usize {
        self.values()
            .filter(|Syncer { service_id, .. }| service_id.is_some())
            .count()
    }
    fn service_online(&self, key: &(Blockchain, Network)) -> bool {
        self.get(key)
            .map(|Syncer { service_id, .. }| service_id.is_some())
            .unwrap_or(false)
    }
    fn pair_ready(&self, coin0: (Blockchain, Network), coin1: (Blockchain, Network)) -> bool {
        SyncerPair::new(self, coin0, coin1)
            .map(|syncer_pair| syncer_pair.ready())
            .unwrap_or(false)
    }
}

struct Syncer {
    // when service_id set, syncer is online
    service_id: Option<ServiceId>,
    clients: HashSet<ServiceId>, // swapds
}

struct SyncerPair<'a> {
    arbitrating_syncer: &'a Syncer,
    accordant_syncer: &'a Syncer,
}

impl<'a> SyncerPair<'a> {
    fn ready(&self) -> bool {
        self.arbitrating_syncer.service_id.is_some() && self.accordant_syncer.service_id.is_some()
    }
    fn new(
        ss: &'a Syncers,
        arbitrating_ix: (Blockchain, Network),
        accordant_ix: (Blockchain, Network),
    ) -> Option<Self> {
        let arbitrating_syncer = ss.get(&arbitrating_ix)?;
        let accordant_syncer = ss.get(&accordant_ix)?;
        Some(SyncerPair {
            arbitrating_syncer,
            accordant_syncer,
        })
    }
}

fn normalize_syncer_services_pair(source: &ServiceId) -> (ServiceId, ServiceId) {
    match source {
        ServiceId::Syncer(Blockchain::Monero, network)
        | ServiceId::Syncer(Blockchain::Bitcoin, network) => (
            ServiceId::Syncer(Blockchain::Bitcoin, *network),
            ServiceId::Syncer(Blockchain::Monero, *network),
        ),
        _ => unreachable!("Not Bitcoin nor Monero syncers"),
    }
}
