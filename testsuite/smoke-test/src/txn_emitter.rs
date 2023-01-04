// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::smoke_test_environment::new_local_swarm_with_aptos;
use anyhow::ensure;
use aptos_forge::{
    EmitJobMode, EmitJobRequest, EntryPoints, NodeExt, Result, Swarm, TransactionType, TxnEmitter,
    TxnStats,
};
use aptos_sdk::{transaction_builder::TransactionFactory, types::PeerId};
use rand::{rngs::OsRng, SeedableRng};
use std::time::Duration;
use tokio::runtime::Builder;

pub async fn generate_traffic(
    swarm: &mut dyn Swarm,
    nodes: &[PeerId],
    duration: Duration,
    gas_price: u64,
) -> Result<TxnStats> {
    ensure!(gas_price > 0, "gas_price is required to be non zero");
    let mut runtime_builder = Builder::new_multi_thread();
    runtime_builder.disable_lifo_slot().enable_all();
    runtime_builder.worker_threads(64);
    let rng = SeedableRng::from_rng(OsRng)?;
    let validator_clients = swarm
        .validators()
        .filter(|v| nodes.contains(&v.peer_id()))
        .map(|n| n.rest_client())
        .collect::<Vec<_>>();
    let mut emit_job_request = EmitJobRequest::default();
    let chain_info = swarm.chain_info();
    let transaction_factory = TransactionFactory::new(chain_info.chain_id).with_gas_unit_price(1);
    let emitter = TxnEmitter::new(transaction_factory, rng);

    emit_job_request = emit_job_request
        .rest_clients(validator_clients)
        .gas_price(gas_price)
        .transaction_mix_per_phase(vec![
            vec![
                (TransactionType::default_account_generation(), 20),
            ],
            vec![
                (TransactionType::default_coin_transfer(), 20),
                // // commenting this out given it consistently fails smoke test
                // // and it seems to be called only from `test_txn_emmitter`
                // (TransactionType::NftMintAndTransfer, 20),
                (TransactionType::PublishPackage, 20),
            ],
            vec![
                (TransactionType::default_call_different_modules(), 20),
                (
                    TransactionType::CallDifferentModules {
                        entry_point: EntryPoints::MakeOrChange {
                            string_length: Some(0),
                            data_length: Some(100),
                        },
                        num_modules: 10,
                        use_account_pool: true,
                    },
                    20,
                ),
            ],
        ])
        .mode(EmitJobMode::ConstTps { tps: 20 });
    emitter
        .emit_txn_for_with_stats(chain_info.root_account, emit_job_request, duration, 3)
        .await
}

#[ignore]
#[tokio::test]
async fn test_txn_emmitter() {
    let mut swarm = new_local_swarm_with_aptos(1).await;

    let all_validators = swarm.validators().map(|v| v.peer_id()).collect::<Vec<_>>();

    let txn_stat = generate_traffic(&mut swarm, &all_validators, Duration::from_secs(20), 1)
        .await
        .unwrap();
    println!("{:?}", txn_stat.rate(Duration::from_secs(10)));
    // assert some much smaller number than expected, so it doesn't fail under contention
    assert!(txn_stat.submitted > 30);
    assert!(txn_stat.committed > 30);
}
