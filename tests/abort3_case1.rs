#![cfg(feature = "integration-test")]
use bitcoin::Amount;
use coinswap::{
    maker::{start_maker_server, MakerBehavior},
    taker::{SwapParams, TakerBehavior},
    utill::ConnectionType,
};

mod test_framework;
use log::{info, warn};
use std::{
    fs::File,
    io::Read,
    path::PathBuf,
    sync::{atomic::Ordering::Relaxed, Arc},
    thread,
    time::Duration,
};
use test_framework::*;

/// ABORT 3: Maker Drops After Setup
/// Case 1: CloseAtContractSigsForRecvrAndSender
///
/// Maker closes connection after receiving a `RespContractSigsForRecvrAndSender` and doesn't broadcasts it's funding txs.
/// Taker wait until a timeout (10ses for test, 5mins for prod) and starts recovery after that.
// This is problematic. Needs more detailed thought.
#[test]
fn abort3_case1_close_at_contract_sigs_for_recvr_and_sender() {
    // ---- Setup ----

    // 6102 is naughty. And theres not enough makers.
    let makers_config_map = [
        (
            (6102, None),
            MakerBehavior::CloseAtContractSigsForRecvrAndSender,
        ),
        ((16102, None), MakerBehavior::Normal),
    ];

    // Initiate test framework, Makers.
    // Taker has normal behavior.
    let (test_framework, mut taker, makers, directory_server_instance, block_generation_handle) =
        TestFramework::init(
            makers_config_map.into(),
            TakerBehavior::Normal,
            ConnectionType::CLEARNET,
        );

    warn!("Running Test: Maker closes connection after receiving a ContractSigsForRecvrAndSender");

    // Fund the Taker  with 3 utxos of 0.05 btc each and do basic checks on the balance
    let org_taker_spend_balance = fund_and_verify_taker(
        &mut taker,
        &test_framework.bitcoind,
        3,
        Amount::from_btc(0.05).unwrap(),
    );

    // Fund the Maker with 4 utxos of 0.05 btc each and do basic checks on the balance.
    let makers_ref = makers.iter().map(Arc::as_ref).collect::<Vec<_>>();
    fund_and_verify_maker(
        makers_ref,
        &test_framework.bitcoind,
        4,
        Amount::from_btc(0.05).unwrap(),
    );

    //  Start the Maker Server threads
    info!("Initiating Maker...");

    let maker_threads = makers
        .iter()
        .map(|maker| {
            let maker_clone = maker.clone();
            thread::spawn(move || {
                start_maker_server(maker_clone).unwrap();
            })
        })
        .collect::<Vec<_>>();

    // Makers take time to fully setup.
    let org_maker_spend_balances = makers
        .iter()
        .map(|maker| {
            while !maker.is_setup_complete.load(Relaxed) {
                info!("Waiting for maker setup completion");
                // Introduce a delay of 10 seconds to prevent write lock starvation.
                thread::sleep(Duration::from_secs(10));
                continue;
            }

            // Check balance after setting up maker server.
            let wallet = maker.wallet.read().unwrap();
            let all_utxos = wallet.get_all_utxo().unwrap();

            let seed_balance = wallet.balance_descriptor_utxo(Some(&all_utxos)).unwrap();

            let fidelity_balance = wallet.balance_fidelity_bonds(Some(&all_utxos)).unwrap();

            let swapcoin_balance = wallet
                .balance_incoming_swap_coins(Some(&all_utxos))
                .unwrap();

            let live_contract_balance = wallet.balance_live_contract(Some(&all_utxos)).unwrap();

            assert_eq!(seed_balance, Amount::from_btc(0.14999).unwrap());
            assert_eq!(fidelity_balance, Amount::from_btc(0.05).unwrap());
            assert_eq!(swapcoin_balance, Amount::ZERO);
            assert_eq!(live_contract_balance, Amount::ZERO);

            seed_balance + swapcoin_balance
        })
        .collect::<Vec<_>>();

    // Initiate Coinswap
    info!("Initiating coinswap protocol");

    // Swap params for coinswap.
    let swap_params = SwapParams {
        send_amount: Amount::from_sat(500000),
        maker_count: 2,
        tx_count: 3,
        required_confirms: 1,
    };
    taker.do_coinswap(swap_params).unwrap();

    // After Swap is done,  wait for maker threads to conclude.
    makers
        .iter()
        .for_each(|maker| maker.shutdown.store(true, Relaxed));

    maker_threads
        .into_iter()
        .for_each(|thread| thread.join().unwrap());

    info!("All coinswaps processed successfully. Transaction complete.");

    // Shutdown Directory Server
    directory_server_instance.shutdown.store(true, Relaxed);

    thread::sleep(Duration::from_secs(10));

    // -------- Fee Tracking and Workflow --------
    //
    // Case 1: Maker6102 is the First Maker, and the Taker recovers from an initiated swap.
    // Workflow: Taker -> Maker6102 (CloseAtContractSigsForRecvrAndSender)
    //
    // | Participant    | Amount Received (Sats) | Amount Forwarded (Sats) | Fee (Sats) | Funding Mining Fees (Sats) | Total Fees (Sats) |
    // |----------------|------------------------|-------------------------|------------|----------------------------|-------------------|
    // | **Taker**      | _                      | 500,000                 | _          | 3,000                      | 3,000             |
    //
    // - Taker forwards [ProofOfFunding] to Maker6102, receives [ReqContractSigsAsRecvrAndSender].
    // - Maker6102 reaches CloseAtContractSigsForRecvrAndSender and doesn’t broadcast funding tx.
    // - Taker recovers from the swap.
    //
    // Final Outcome for Taker (Recover from Swap):
    //
    // | Participant    | Mining Fee for Contract txes (Sats) | Timelock Fee (Sats) | Funding Fee (Sats) | Total Recovery Fees (Sats) |
    // |----------------|------------------------------------|---------------------|--------------------|----------------------------|
    // | **Taker**      | 3,000                              | 768                 | 3,000              | 6,768                      |
    //
    // - Taker recovers funds but loses **6,768 sats** in mining fees.
    //
    // Final Outcome for Makers (Case 1):
    //
    // | Participant    | Coinswap Outcome (Sats)                 |
    // |----------------|------------------------------------------|
    // | **Maker6102**  | 0 (Marked as a bad maker by the Taker)   |
    // | **Maker16102** | 0                                        |
    //
    // Case 2: Maker6102 is the Second Maker.
    // Workflow: Taker -> Maker16102 -> Maker6102 (CloseAtContractSigsForRecvrAndSender)
    //
    // | Participant    | Amount Received (Sats) | Amount Forwarded (Sats) | Fee (Sats) | Funding Mining Fees (Sats) | Total Fees (Sats) |
    // |----------------|------------------------|-------------------------|------------|----------------------------|-------------------|
    // | **Taker**      | _                      | 500,000                 | _          | 3,000                      | 3,000             |
    // | **Maker16102** | 500,000                | 463,500                 | 33,500     | 3,000                      | 36,500            |
    //
    // - Maker6102 receives [ProofOfFunding] of Maker16102, sends [ReqContractSigsAsRecvrAndSender].
    // - Maker6102 reaches CloseAtContractSigsForRecvrAndSender and doesn’t broadcast funding tx.
    //
    // - After timeout, Taker and Maker16102 recover funds but lose **6,768 sats** each in fees.
    //
    // Final Outcome for Maker6102:
    //
    // | Participant    | Coinswap Outcome (Sats)                 |
    // |----------------|------------------------------------------|
    // | **Maker6102**  | 0 (Marked as a bad maker by the Taker)   |
    //
    // Final Outcome for Maker16102 and Taker:
    //
    // | Participant    | Mining Fee for Contract txes (Sats) | Timelock Fee (Sats) | Funding Fee (Sats) | Total Recovery Fees (Sats) |
    // |----------------|------------------------------------|---------------------|--------------------|----------------------------|
    // | **Taker**      | 3,000                              | 768                 | 3,000              | 6,768                      |
    // | **Maker16102** | 3,000                              | 768                 | 3,000              | 6,768                      |

    // Maker6102 gets banned for being naughty.
    match taker.config.connection_type {
        ConnectionType::CLEARNET => {
            assert_eq!(
                format!("127.0.0.1:{}", 6102),
                taker.get_bad_makers()[0].address.to_string()
            );
        }
        #[cfg(feature = "tor")]
        ConnectionType::TOR => {
            let onion_addr_path =
                PathBuf::from(format!("/tmp/tor-rust-maker{}/hs-dir/hostname", 6102));
            let mut file = File::open(onion_addr_path).unwrap();
            let mut onion_addr: String = String::new();
            file.read_to_string(&mut onion_addr).unwrap();
            onion_addr.pop();
            assert_eq!(
                format!("{}:{}", onion_addr, 6102),
                taker.get_bad_makers()[0].address.to_string()
            );
        }
    }

    // After Swap checks:
    verify_swap_results(
        &taker,
        &makers,
        org_taker_spend_balance,
        org_maker_spend_balances,
    );

    info!("All checks successful. Terminating integration test case");

    test_framework.stop();
    block_generation_handle.join().unwrap();
}
