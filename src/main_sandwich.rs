mod apis;
mod arbitrage;
mod crawling;
mod dex;
mod evm;
mod formula;
mod multicall;
mod sandwich;
mod types;
mod utils;
mod config;
mod logging;

use apis::subscribe::{stream_new_block_and_pending_txs, stream_new_future_accessible_block_number};
use sandwich::search::search_sandwich;
use apis::transaction::*;
use apis::simple::*;
use crawling::get_pool_list_from_coinmarketcap;
use dex::DEX2CLASS;
use utils::eq_address;

use types::{Config, Transaction, DexInfos, DexInfo, SandwichAttack, ArbitrageAttack};

use crossbeam_channel::Receiver;
use std::{thread, time::Duration};
use std::sync::{Arc};
use std::sync::atomic::{AtomicI32, Ordering};
use tracing::{info, error};
use anyhow::Result;

use std::{
    sync::{Arc},
    sync::atomic::{AtomicI32, Ordering},
    thread,
};

use crossbeam_channel::bounded;
use tracing::info;

use crate::{
    apis::subscribe::{stream_new_block_and_pending_txs, stream_new_future_accessible_block_number},
    config::bsc_gcp_config,
    logging,
    types::{Config, Transaction},
};


fn get_dex_infos_from_pool_list(pool_list_by_dex: &std::collections::HashMap<String, Vec<types::PoolInfo>>) -> DexInfos {
    info!("Get dex infos from pool list");
    let dex_infos: Vec<DexInfo> = pool_list_by_dex
        .iter()
        .map(|(dex, pool_list)| DexInfo::new(dex.clone(), pool_list.clone()))
        .collect();

    DexInfos::new(dex_infos)
}

fn set_token_addresses_to_dex_infos(http_endpoint: &str, dex_infos: &mut DexInfos) -> DexInfos {
    info!("Set token addresses to dex infos");
    for dex_info in dex_infos.iter_mut() {
        let dex_instance = DEX2CLASS::get(&dex_info.dex).unwrap().build(http_endpoint);
        let token_addresses = dex_instance.fetch_pools_token_addresses(&dex_info.get_all_pool_address());

        for pool_info in dex_info.pools.iter_mut() {
            if let Some(tokens) = token_addresses.get(&pool_info.address) {
                pool_info.token_addresses = tokens.clone();
            }
        }
    }
    dex_infos.clone()
}

fn main_worker(cfg: Config, rx: Receiver<Transaction>, accessible_block_number: Arc<AtomicI32>) {
    let mut block_number = get_block_number(&cfg.http_endpoint);
    let mut timestamp = get_timestamp(&cfg.http_endpoint, block_number);

    loop {
        if let Ok(tx) = rx.try_recv() {
            if timestamp + 3.0 < utils::now_timestamp() {
                loop {
                    let new_block_number = get_block_number(&cfg.http_endpoint);
                    if new_block_number > block_number {
                        block_number = new_block_number;
                        timestamp = get_timestamp(&cfg.http_endpoint, block_number);
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }

            let Ok((mut sandwich_attack, arbitrage_attack, found_block)) =
                search_sandwich(&cfg, &tx) else {
                    info!("Search failed or attack not found");
                    continue;
            };

            if found_block < get_block_number(&cfg.http_endpoint) {
                info!("[{}] Block too old: {}", tx.tx_hash, found_block);
                continue;
            }

            if sandwich_attack.is_none() {
                info!("No sandwich attack found");
                continue;
            }

            let sandwich_attack = sandwich_attack.unwrap();

            if timestamp + 3.0 < utils::now_timestamp() {
                info!("[{}] Timestamp is too old: {}", tx.tx_hash, timestamp);
                continue;
            }

            if !is_pending_tx(&cfg.http_endpoint, &tx.tx_hash) {
                info!("[{}] Not pending", tx.tx_hash);
                continue;
            }

            let block_val = accessible_block_number.load(Ordering::Relaxed);
            if block_val > 0 {
                info!("Use bloXroute");

                let back_run_gas_price = ((tx.gas + sandwich_attack.back_run_gas_used)
                    / sandwich_attack.back_run_gas_used)
                    * 1_000_000_000;
                let bundle_fee = ((sandwich_attack.revenue_based_on_eth
                    - 1_000_000_000 * sandwich_attack.front_run_gas_used
                    - back_run_gas_price * sandwich_attack.back_run_gas_used)
                    * 98)
                    / 100;

                if bundle_fee < 1_000_000_000 {
                    info!("Bundle fee too low: {}", bundle_fee);
                    continue;
                }

                sandwich_attack.set_back_run_function("sandwichBackRunWithBloxroute");
                sandwich_attack.trim_front_run_data();

                let _ = send_sandwich_attack_using_bloxroute(
                    &cfg,
                    &tx,
                    &sandwich_attack,
                    block_val,
                    back_run_gas_price,
                    bundle_fee,
                );

                info!("[{}] Gas price: {}", tx.tx_hash, back_run_gas_price);
                info!("[{}] Bundle fee: {}", tx.tx_hash, bundle_fee);
            } else if block_val == -1 {
                info!("Use 48club");
                let gas_price = ((sandwich_attack.revenue_based_on_eth
                    - ((tx.gas_price + 1) * sandwich_attack.front_run_gas_used
                        + 1_000_000_000 * sandwich_attack.back_run_gas_used))
                    * 98)
                    / (100 * 21000);

                if gas_price < 1_000_000_000 {
                    info!("Gas price too low: {}", gas_price);
                    continue;
                }

                let min_gas = get_48_club_minimum_gas_price().unwrap();
                if gas_price < min_gas {
                    info!(
                        "48club gas price too low: {} < {}",
                        gas_price, min_gas
                    );
                    continue;
                }

                sandwich_attack.set_back_run_function("sandwichBackRun");
                sandwich_attack.trim_front_run_data();

                let _ = send_sandwich_attack_using_48club(&cfg, &tx, &sandwich_attack, gas_price);

                info!("[{}] Gas price: {}", tx.tx_hash, gas_price);
            } else {
                info!("Use General");

                if arbitrage_attack
                    .as_ref()
                    .map(|a| a.revenue_based_on_eth > 10u128.pow(16))
                    .unwrap_or(false)
                {
                    info!("[{}] Arbitrage found", tx.tx_hash);
                    continue;
                }

                let expected_profit = sandwich_attack.revenue_based_on_eth
                    - ((tx.gas_price + 1) * sandwich_attack.front_run_gas_used
                        + tx.gas_price * sandwich_attack.back_run_gas_used);
                if expected_profit < 10u128.pow(15) {
                    info!("[{}] Not profitable", tx.tx_hash);
                    continue;
                }

                let front_run_gas_price = std::cmp::max(
                    tx.gas_price + 1,
                    ((expected_profit * 3) / 10) / sandwich_attack.front_run_gas_used,
                );

                sandwich_attack.set_front_run_function("sandwichFrontRunDifficult");
                sandwich_attack.set_back_run_function("sandwichBackRunDifficult");
                sandwich_attack.back_run_data.push(block_number + 1);

                delay_time(&cfg.http_endpoint, block_number);

                let (front_tx, back_tx) = send_sandwich_attack_using_general(
                    &cfg,
                    &tx,
                    &sandwich_attack,
                    front_run_gas_price,
                )
                .unwrap();

                while is_pending_tx(&cfg.http_endpoint, &back_tx) {
                    thread::sleep(Duration::from_secs(1));
                    info!("[{}] Waiting for back run tx", tx.tx_hash);
                }

                if is_successful_tx(&cfg.http_endpoint, &front_tx)
                    && !is_successful_tx(&cfg.http_endpoint, &back_tx)
                {
                    if eq_address(
                        &sandwich_attack.front_run_data[3].last().unwrap(),
                        &cfg.wrapped_native_token_address,
                    ) {
                        info!("[{}] Recovered with same token", tx.tx_hash);
                        continue;
                    }

                    info!("[{}] Recovering failed back run", tx.tx_hash);
                    sandwich_attack.set_back_run_function("sandwichBackRun");
                    sandwich_attack.back_run_data.truncate(4);
                    let _ = send_recover_sandwich_attack(&cfg, &sandwich_attack);
                }
            }

            info!("[{}] Block ref: {}", tx.tx_hash, block_val);
            info!("Sandwich attack: {:?}", sandwich_attack);
            info!("Victim tx: {}", tx.tx_hash);
        } else {
            thread::sleep(Duration::from_micros(100));
        }
    }
}


fn spawn_block_and_pending_tx_runner(cfg: Config, tx_sender: crossbeam_channel::Sender<Transaction>) {
    thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(stream_new_block_and_pending_txs(cfg, tx_sender));
    });
}

fn spawn_future_block_number_runner(cfg: Config, accessible_block_number: Arc<AtomicI32>) {
    thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(stream_new_future_accessible_block_number(cfg, accessible_block_number));
    });
}

fn main() {
    logging::init_logger();
    let cfg = bsc_gcp_config();

    // Initialize bounded queue
    let (tx_sender, tx_receiver) = bounded::<Transaction>(40);

    // Shared block number (like multiprocessing.Value)
    let accessible_block_number = Arc::new(AtomicI32::new(0));

    // Start multiple worker threads
    let mut worker_threads = Vec::new();
    let worker_num = 6;

    for _ in 0..worker_num {
        let cfg = cfg.clone();
        let rx = tx_receiver.clone();
        let block_ref = Arc::clone(&accessible_block_number);
        let handle = thread::spawn(move || {
            crate::main_worker(cfg, rx, block_ref);
        });
        worker_threads.push(handle);
    }

    // Start async streamers
    spawn_block_and_pending_tx_runner(cfg.clone(), tx_sender.clone());
    spawn_future_block_number_runner(cfg.clone(), Arc::clone(&accessible_block_number));

    // Join all workers
    for worker in worker_threads {
        worker.join().unwrap();
    }
}

