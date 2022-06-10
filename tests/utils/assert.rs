//! Serie of helper assertion functions.

use farcaster_node::rpc::Request;
use farcaster_node::syncerd::types::Event;
use farcaster_node::syncerd::TaskId;
use farcaster_node::syncerd::{FeeEstimation, FeeEstimations};

pub fn address_transaction(request: Request, expected_amount: u64, possible_txids: Vec<Vec<u8>>) {
    match request {
        Request::SyncerdBridgeEvent(event) => match event.event {
            Event::AddressTransaction(address_transaction) => {
                assert_eq!(address_transaction.amount, expected_amount);
                assert!(possible_txids.contains(&address_transaction.hash));
            }
            _ => panic!("expected address transaction event"),
        },
        _ => panic!("expected syncerd bridge event"),
    }
}

pub fn sweep_success(request: Request, id: TaskId) {
    match request {
        Request::SyncerdBridgeEvent(event) => match event.event {
            Event::SweepSuccess(sweep_success) => {
                assert_eq!(sweep_success.id, id);
            }
            _ => panic!("expected address transaction event"),
        },
        _ => panic!("expected syncerd bridge event"),
    }
}

pub fn received_height_changed(request: Request, expected_height: u64) {
    match request {
        Request::SyncerdBridgeEvent(event) => match event.event {
            Event::HeightChanged(height_changed) => {
                assert_eq!(height_changed.height, expected_height);
            }
            _ => {
                panic!("expected height changed event");
            }
        },
        _ => {
            panic!("expected syncerd bridge event");
        }
    }
}

pub fn transaction_confirmations(
    request: Request,
    expected_confirmations: Option<u32>,
    expected_block_hash: Vec<u8>,
) {
    match request {
        Request::SyncerdBridgeEvent(event) => match event.event {
            Event::TransactionConfirmations(transaction_confirmations) => {
                assert_eq!(
                    transaction_confirmations.confirmations,
                    expected_confirmations
                );
                assert_eq!(transaction_confirmations.block, expected_block_hash);
            }
            _ => panic!("expected address transaction event"),
        },
        _ => panic!("expected syncerd bridge event"),
    }
}

pub fn task_aborted(request: Request, expected_error: Option<String>, mut expected_id: Vec<u32>) {
    match request {
        Request::SyncerdBridgeEvent(event) => match event.event {
            Event::TaskAborted(mut task_aborted) => {
                assert_eq!(
                    &task_aborted.id.sort_unstable(),
                    &expected_id.sort_unstable()
                );
                assert_eq!(task_aborted.error, expected_error);
            }
            _ => {
                panic!("expected task aborted event");
            }
        },
        _ => {
            panic!("expected syncerd bridge event");
        }
    }
}

pub fn transaction_broadcasted(request: Request, has_error: bool, error_msg: Option<String>) {
    match request {
        Request::SyncerdBridgeEvent(event) => match event.event {
            Event::TransactionBroadcasted(transaction_broadcasted) => {
                if has_error {
                    assert!(transaction_broadcasted.error.is_some());
                    if error_msg.is_some() {
                        assert_eq!(transaction_broadcasted.error.unwrap(), error_msg.unwrap());
                    }
                } else {
                    assert!(transaction_broadcasted.error.is_none());
                }
            }
            _ => {
                panic!("expected height changed event");
            }
        },
        _ => {
            panic!("expected syncerd bridge event");
        }
    }
}

pub fn transaction_received(request: Request, expected_txid: bitcoin::Txid) {
    match request {
        Request::SyncerdBridgeEvent(event) => match event.event {
            Event::TransactionRetrieved(transaction) => {
                assert_eq!(transaction.tx.unwrap().txid(), expected_txid);
            }
            _ => {
                panic!("expected height changed event");
            }
        },
        _ => {
            panic!("expected syncerd bridge event");
        }
    }
}

pub fn fee_estimation_received(request: Request) {
    match request {
        Request::SyncerdBridgeEvent(farcaster_node::rpc::request::SyncerdBridgeEvent {
                         event:
                Event::FeeEstimation(FeeEstimation {
                    fee_estimations:
                        FeeEstimations::BitcoinFeeEstimation {
                            high_priority_sats_per_kvbyte,
                            low_priority_sats_per_kvbyte,
                        },
                    ..
                }),
            ..
        }) => {
             assert!(high_priority_sats_per_kvbyte >= 1000);
            assert!(low_priority_sats_per_kvbyte >= 1000);
        }
        _ => {
            panic!("expected syncerd bridge event");
        }
    }
}
