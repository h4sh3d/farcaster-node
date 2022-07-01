use farcaster_core::{
    negotiation::PublicOffer,
    role::{SwapRole, TradeRole},
    swap::btcxmr::BtcXmr,
};
use strict_encoding::{StrictDecode, StrictEncode};

use crate::rpc::request::{Commit, Outcome, Params};

#[derive(Display, Debug, Clone, StrictEncode, StrictDecode)]
pub enum AliceState {
    // #[display("Start: {0:#?} {1:#?}")]
    #[display("Start")]
    StartA {
        local_trade_role: TradeRole,
        public_offer: PublicOffer<BtcXmr>,
    }, // local, both
    // #[display("Commit: {0}")]
    #[display("Commit")]
    CommitA {
        local_trade_role: TradeRole,
        local_params: Params,
        remote_params: Option<Params>,
        local_commit: Commit,
        remote_commit: Option<Commit>,
    }, // local, local, local, remote
    // #[display("Reveal: {0:#?}")]
    #[display("Reveal")]
    RevealA {
        local_params: Params,
        remote_commit: Commit,
        remote_params: Option<Params>,
    }, // local, remote, remote
    #[display("RefundSigs(xmr_locked({xmr_locked}), buy_pub({buy_published}), cancel_seen({cancel_seen}), refund_seen({refund_seen}))")]
    RefundSigA {
        xmr_locked: bool,
        buy_published: bool,
        cancel_seen: bool,
        refund_seen: bool,
        remote_params: Params,
        /* #[display("local_view_share({0})")] */
        local_params: Params,
    },
    #[display("Finish({0})")]
    FinishA(Outcome),
}

#[derive(Display, Debug, Clone, StrictEncode, StrictDecode)]
pub enum BobState {
    // #[display("Start {0:#?} {1:#?}")]
    #[display("Start")]
    StartB {
        local_trade_role: TradeRole,
        public_offer: PublicOffer<BtcXmr>,
    }, // local, both
    // #[display("Commit {0} {1}")]
    #[display("Commit")]
    CommitB {
        local_trade_role: TradeRole,
        local_params: Params,
        remote_params: Option<Params>,
        local_commit: Commit,
        remote_commit: Option<Commit>,
        b_address: bitcoin::Address,
    },
    // #[display("Reveal: {0:#?}")]
    #[display("Reveal")]
    RevealB {
        local_params: Params,
        remote_commit: Commit,
        b_address: bitcoin::Address,
        local_trade_role: TradeRole,
        remote_params: Option<Params>,
    }, // local, remote, local, ..missing, remote
    // #[display("CoreArb: {0:#?}")]
    #[display("CoreArb")]
    CorearbB {
        received_refund_procedure_signatures: bool,
        cancel_seen: bool,
        remote_params: Params,
        local_params: Params,
        b_address: bitcoin::Address,
    }, // lock (not signed), cancel_seen, remote
    #[display("BuySig")]
    BuySigB { buy_tx_seen: bool },
    #[display("Finish({0})")]
    FinishB(Outcome),
}

#[derive(Display, Debug, Clone, StrictEncode, StrictDecode)]
#[display(inner)]
pub enum State {
    #[display("AliceState({0})")]
    Alice(AliceState),
    #[display("BobState({0})")]
    Bob(BobState),
}

// The state impl is not public and may contain code not used yet, we can relax the linter and
// allow dead code.
#[allow(dead_code)]
impl State {
    pub fn swap_role(&self) -> SwapRole {
        match self {
            State::Alice(_) => SwapRole::Alice,
            State::Bob(_) => SwapRole::Bob,
        }
    }
    pub fn remote_params(&self) -> Option<Params> {
        match self {
            State::Alice(AliceState::CommitA { remote_params, .. })
            | State::Alice(AliceState::RevealA { remote_params, .. })
            | State::Bob(BobState::CommitB { remote_params, .. })
            | State::Bob(BobState::RevealB { remote_params, .. }) => remote_params.clone(),

            State::Alice(AliceState::RefundSigA { remote_params, .. })
            | State::Bob(BobState::CorearbB { remote_params, .. }) => Some(remote_params.clone()),

            _ => None,
        }
    }
    pub fn sup_remote_params(&mut self, params: Params) -> bool {
        match self {
            State::Alice(AliceState::CommitA { remote_params, .. })
            | State::Alice(AliceState::RevealA { remote_params, .. })
            | State::Bob(BobState::CommitB { remote_params, .. })
            | State::Bob(BobState::RevealB { remote_params, .. })
                if remote_params.is_none() =>
            {
                *remote_params = Some(params);
                true
            }
            _ => false,
        }
    }
    pub fn a_xmr_locked(&self) -> bool {
        if let State::Alice(AliceState::RefundSigA { xmr_locked, .. }) = self {
            *xmr_locked
        } else {
            false
        }
    }
    pub fn a_buy_published(&self) -> bool {
        if let State::Alice(AliceState::RefundSigA { buy_published, .. }) = self {
            *buy_published
        } else {
            false
        }
    }
    pub fn a_refund_seen(&self) -> bool {
        if let State::Alice(AliceState::RefundSigA { refund_seen, .. }) = self {
            *refund_seen
        } else {
            false
        }
    }
    pub fn cancel_seen(&self) -> bool {
        if let State::Bob(BobState::CorearbB { cancel_seen, .. })
        | State::Alice(AliceState::RefundSigA { cancel_seen, .. }) = self
        {
            *cancel_seen
        } else {
            false
        }
    }
    pub fn sup_cancel_seen(&mut self) -> bool {
        match self {
            State::Alice(AliceState::RefundSigA { cancel_seen, .. })
            | State::Bob(BobState::CorearbB { cancel_seen, .. }) => {
                *cancel_seen = true;
                true
            }
            _ => false,
        }
    }
    pub fn sup_received_refund_procedure_signatures(&mut self) -> bool {
        match self {
            State::Bob(BobState::CorearbB {
                received_refund_procedure_signatures,
                ..
            }) => {
                *received_refund_procedure_signatures = true;
                true
            }
            _ => false,
        }
    }
    pub fn b_received_refund_procedure_signatures(&self) -> bool {
        if let State::Bob(BobState::CorearbB {
            received_refund_procedure_signatures,
            ..
        }) = self
        {
            *received_refund_procedure_signatures
        } else {
            false
        }
    }
    pub fn a_start(&self) -> bool {
        matches!(self, State::Alice(AliceState::StartA { .. }))
    }
    pub fn a_commit(&self) -> bool {
        matches!(self, State::Alice(AliceState::CommitA { .. }))
    }
    pub fn a_reveal(&self) -> bool {
        matches!(self, State::Alice(AliceState::RevealA { .. }))
    }
    pub fn b_start(&self) -> bool {
        matches!(self, State::Bob(BobState::StartB { .. }))
    }
    pub fn b_commit(&self) -> bool {
        matches!(self, State::Bob(BobState::CommitB { .. }))
    }
    pub fn b_reveal(&self) -> bool {
        matches!(self, State::Bob(BobState::RevealB { .. }))
    }
    pub fn b_core_arb(&self) -> bool {
        matches!(self, State::Bob(BobState::CorearbB { .. }))
    }
    pub fn b_buy_sig(&self) -> bool {
        matches!(self, State::Bob(BobState::BuySigB { .. }))
    }
    pub fn b_outcome_cancel(&self) -> bool {
        matches!(self, State::Bob(BobState::FinishB(Outcome::Cancel)))
    }
    pub fn remote_commit(&self) -> Option<&Commit> {
        match self {
            State::Alice(AliceState::CommitA { remote_commit, .. })
            | State::Bob(BobState::CommitB { remote_commit, .. }) => remote_commit.as_ref(),
            State::Alice(AliceState::RevealA { remote_commit, .. })
            | State::Bob(BobState::RevealB { remote_commit, .. }) => Some(remote_commit),
            _ => None,
        }
    }
    pub fn local_params(&self) -> Option<&Params> {
        match self {
            State::Alice(AliceState::CommitA { local_params, .. })
            | State::Bob(BobState::CommitB { local_params, .. })
            | State::Alice(AliceState::RevealA { local_params, .. })
            | State::Alice(AliceState::RefundSigA { local_params, .. })
            | State::Bob(BobState::RevealB { local_params, .. })
            | State::Bob(BobState::CorearbB { local_params, .. }) => Some(local_params),
            _ => None,
        }
    }
    pub fn public_offer(&self) -> Option<&PublicOffer<BtcXmr>> {
        match self {
            State::Alice(AliceState::StartA { public_offer, .. })
            | State::Bob(BobState::StartB { public_offer, .. }) => Some(public_offer),
            _ => None,
        }
    }
    pub fn b_address(&self) -> Option<&bitcoin::Address> {
        match self {
            State::Bob(BobState::CommitB { b_address, .. })
            | State::Bob(BobState::RevealB { b_address, .. })
            | State::Bob(BobState::CorearbB { b_address, .. }) => Some(b_address),
            _ => None,
        }
    }
    pub fn local_commit(&self) -> Option<&Commit> {
        match self {
            State::Bob(BobState::CommitB { local_commit, .. })
            | State::Alice(AliceState::CommitA { local_commit, .. }) => Some(local_commit),
            _ => None,
        }
    }
    pub fn commit(&self) -> bool {
        matches!(
            self,
            State::Alice(AliceState::CommitA { .. }) | State::Bob(BobState::CommitB { .. })
        )
    }
    pub fn reveal(&self) -> bool {
        matches!(
            self,
            State::Alice(AliceState::RevealA { .. }) | State::Bob(BobState::RevealB { .. })
        )
    }
    pub fn a_refundsig(&self) -> bool {
        matches!(self, State::Alice(AliceState::RefundSigA { .. }))
    }
    pub fn b_buy_tx_seen(&self) -> bool {
        if !self.b_buy_sig() {
            return false;
        }
        match self {
            State::Bob(BobState::BuySigB { buy_tx_seen }) => *buy_tx_seen,
            _ => unreachable!("conditional early return"),
        }
    }
    /// returns whether safe to cancel given swap role & current stage of swap protocol
    pub fn safe_cancel(&self) -> bool {
        if self.finish() || self.cancel_seen() || self.a_refund_seen() {
            return false;
        }
        match self.swap_role() {
            SwapRole::Alice => self.a_refundsig() && !self.a_buy_published() && !self.cancel_seen(),
            SwapRole::Bob => {
                (self.b_core_arb() || self.b_buy_sig())
                    && !self.b_buy_tx_seen()
                    && !self.cancel_seen()
            }
        }
    }
    pub fn start(&self) -> bool {
        matches!(
            self,
            State::Alice(AliceState::StartA { .. }) | State::Bob(BobState::StartB { .. })
        )
    }
    pub fn finish(&self) -> bool {
        matches!(
            self,
            State::Alice(AliceState::FinishA(..)) | State::Bob(BobState::FinishB(..))
        )
    }
    pub fn trade_role(&self) -> Option<TradeRole> {
        match self {
            State::Alice(AliceState::StartA {
                local_trade_role, ..
            })
            | State::Bob(BobState::StartB {
                local_trade_role, ..
            })
            | State::Alice(AliceState::CommitA {
                local_trade_role, ..
            })
            | State::Bob(BobState::CommitB {
                local_trade_role, ..
            })
            | State::Bob(BobState::RevealB {
                local_trade_role, ..
            }) => Some(*local_trade_role),
            _ => None,
        }
    }
    pub fn sup_start_to_commit(
        self,
        local_commit: Commit,
        local_params: Params,
        funding_address: Option<bitcoin::Address>,
        remote_commit: Option<Commit>,
    ) -> Self {
        if !self.start() {
            error!("Not on Start state, not updating state");
            return self;
        }
        let remote_params = None;
        match (self, funding_address) {
            (State::Bob(BobState::StartB{local_trade_role, ..}), Some(b_address)) => {
                State::Bob(BobState::CommitB {
                        local_trade_role,
                        local_params,
                        local_commit,
                        remote_commit,
                        remote_params,
                        b_address,
                    },
                )
            }
            (State::Alice(AliceState::StartA{local_trade_role, ..}), None) => {
                State::Alice(AliceState::CommitA{
                    local_trade_role,
                    local_params,
                    local_commit,
                    remote_commit,
                    remote_params,
                })

            }
            _ => unreachable!(
                "state conditional enforces state is Start: Expects Start state, and funding_address"
            ),
        }
    }
    pub fn sup_commit_to_reveal(self) -> Self {
        if !self.commit() {
            error!("Not on Commit state, not updating state");
            return self;
        }
        if self.remote_commit().is_none() {
            error!("remote commit should be set already");
            return self;
        }
        match self {
            State::Alice(AliceState::CommitA {
                local_params,
                remote_commit: Some(remote_commit),
                remote_params,
                ..
            }) => State::Alice(AliceState::RevealA {
                local_params,
                remote_commit,
                remote_params,
            }),

            State::Bob(BobState::CommitB {
                local_params,
                remote_commit: Some(remote_commit),
                local_trade_role,
                remote_params,
                b_address,
                ..
            }) => State::Bob(BobState::RevealB {
                local_params,
                remote_commit,
                b_address,
                local_trade_role,
                remote_params,
            }),

            _ => unreachable!("checked state on pattern to be Commit"),
        }
    }
    pub fn t_sup_remote_commit(&mut self, commit: Commit) -> bool {
        if !self.commit() {
            error!("Not on Commit state, not updating state");
            return false;
        }
        if self.remote_commit().is_some() {
            error!("remote commit already set");
            return false;
        }

        match self {
            State::Alice(AliceState::CommitA { remote_commit, .. })
            | State::Bob(BobState::CommitB { remote_commit, .. }) => {
                *remote_commit = Some(commit);
            }
            _ => unreachable!("checked state on pattern to be Commit"),
        }
        true
    }

    /// Update Bob BuySig state from XMR unlocked to locked state
    pub fn b_sup_buysig_buy_tx_seen(&mut self) {
        if !self.b_buy_sig() {
            error!(
                "Wrong state, not updating. Expected BuySig, found {}",
                &*self
            );
            return;
        } else if self.b_buy_tx_seen() {
            error!("Buy tx was previously seen, not updating state");
            return;
        }
        match self {
            State::Bob(BobState::BuySigB { buy_tx_seen }) if !(*buy_tx_seen) => *buy_tx_seen = true,
            _ => unreachable!("checked state"),
        }
    }
    /// Update Alice RefundSig state from XMR unlocked to locked state
    pub fn a_sup_refundsig_xmrlocked(&mut self) -> bool {
        if let State::Alice(AliceState::RefundSigA { xmr_locked, .. }) = self {
            if !*xmr_locked {
                trace!("setting xmr_locked");
                *xmr_locked = true;
                true
            } else {
                trace!("xmr_locked was already set to true");
                false
            }
        } else {
            error!("Not on RefundSig state");
            false
        }
    }
    pub fn a_sup_refundsig_refund_seen(&mut self) -> bool {
        if let State::Alice(AliceState::RefundSigA { refund_seen, .. }) = self {
            if !*refund_seen {
                trace!("setting refund_seen");
                *refund_seen = true;
                true
            } else {
                error!("refund_seen was already set to true");
                false
            }
        } else {
            error!("Not on RefundSig state");
            false
        }
    }
}