#![cfg(feature = "integration-test")]
use bitcoin::Amount;
use coinswap::{
    maker::{start_maker_server, MakerBehavior},
    taker::{SwapParams, TakerBehavior},
    utill::ConnectionType,
};
use std::sync::Arc;
mod test_framework;
use test_framework::*;

use std::{sync::atomic::Ordering::Relaxed, thread, time::Duration};

/// Malice 2: Maker Broadcasts contract transactions prematurely.
///
/// The Taker and other Makers identify the situation and gets their money back via contract txs. This is
/// a potential DOS on other Makers. But the attacker Maker would loose money too in the process.
///
/// This case is hard to "blame". As the contract transactions is available to both the Makers, its not identifiable
/// which Maker is the culprit. Taker does not ban in this case.
#[test]
fn malice2_maker_broadcast_contract_prematurely() {
    // ---- Setup ----

    let makers_config_map = [
        ((6102, None), MakerBehavior::BroadcastContractAfterSetup),
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
    log::info!("Initiating Maker...");

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
                log::info!("Waiting for maker setup completion");
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
    log::info!("Initiating coinswap protocol");

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

    log::info!("All coinswaps processed successfully. Transaction complete.");

    // Shutdown Directory Server
    directory_server_instance.shutdown.store(true, Relaxed);

    thread::sleep(Duration::from_secs(10));

    // -------- Fee Tracking and Workflow --------
    //
    // Case 1: Maker6102 is the First Maker.
    // Workflow: Taker -> Maker6102 (BroadcastContractAfterSetup) -> Maker16102
    //
    // | Participant    | Amount Received (Sats) | Amount Forwarded (Sats) | Fee (Sats) | Funding Mining Fees (Sats) | Total Fees (Sats) |
    // |----------------|------------------------|-------------------------|------------|----------------------------|-------------------|
    // | **Taker**      | _                      | 500,000                 | _          | 3,000                      | 3,000             |
    // | **Maker6102**  | 500,000                | 463,500                 | 33,500     | 3,000                      | 36,500            |
    //
    // Maker6102 => BroadcastContractAfterSetup
    //
    // Seeing those contract txes, the Taker recovers from the swap.
    // Taker and Maker6102 recover funds but lose **6,768 sats** each in fees.
    //
    // Final Outcome for Taker & Maker6102:
    // | Participant    | Mining Fee for Contract txes (Sats) | Timelock Fee (Sats) | Funding Fee (Sats) | Total Recovery Fees (Sats) |
    // |----------------|------------------------------------|---------------------|--------------------|----------------------------|
    // | **Taker**      | 3,000                              | 768                 | 3,000              | 6,768                      |
    // | **Maker6102**  | 3,000                              | 768                 | 3,000              | 6,768                      |
    //
    // Final Outcome for Maker16102:
    // | Participant    | Coinswap Outcome (Sats) |
    // |----------------|--------------------------|
    // | **Maker16102** | 0                        |
    //
    // ------------------------------------------------------------------------------------------------------------------------
    //
    // Case 2: Maker6102 is the Last Maker.
    // Workflow: Taker -> Maker16102 -> Maker6102 (BroadcastContractAfterSetup)
    //
    // | Participant    | Amount Received (Sats) | Amount Forwarded (Sats) | Fee (Sats) | Funding Mining Fees (Sats) | Total Fees (Sats) |
    // |----------------|------------------------|-------------------------|------------|----------------------------|-------------------|
    // | **Taker**      | _                      | 500,000                 | _          | 3,000                      | 3,000             |
    // | **Maker16102** | 500,000                | 463,500                 | 33,500     | 3,000                      | 36,500            |
    // | **Maker6102**  | 463,500                | 438,642                 | 21,858     | 3,000                      | 24,858            |
    //
    // Maker6102 => BroadcastContractAfterSetup
    //
    // Participants regain their initial funding amounts but incur a total loss of **6,768 sats**
    // due to mining fees (recovery + initial transaction fees).
    //
    // | Participant    | Mining Fee for Contract txes (Sats) | Timelock Fee (Sats) | Funding Fee (Sats) | Total Recovery Fees (Sats) |
    // |----------------|------------------------------------|---------------------|--------------------|----------------------------|
    // | **Taker**      | 3,000                              | 768                 | 3,000              | 6,768                      |
    // | **Maker16102** | 3,000                              | 768                 | 3,000              | 6,768                      |
    // | **Maker6102**  | 3,000                              | 768                 | 3,000              | 6,768                      |

    // After Swap checks:
    verify_swap_results(
        &taker,
        &makers,
        org_taker_spend_balance,
        org_maker_spend_balances,
    );

    log::info!("All checks successful. Terminating integration test case");

    test_framework.stop();
    block_generation_handle.join().unwrap();
}
