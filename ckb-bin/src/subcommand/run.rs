use crate::helper::{deadlock_detection, wait_for_exit};
use ckb_app_config::{ExitCode, RunArgs};
use ckb_build_info::Version;
use ckb_chain::chain::ChainService;
use ckb_jsonrpc_types::ScriptHashType;
use ckb_logger::info_target;
use ckb_miner::BlockAssembler;
use ckb_network::{CKBProtocol, NetworkService, NetworkState};
use ckb_network_alert::alert_relayer::AlertRelayer;
use ckb_notify::NotifyService;
use ckb_resource::Resource;
use ckb_rpc::{RpcServer, ServiceBuilder};
use ckb_shared::shared::{Shared, SharedBuilder};
use ckb_sync::{NetTimeProtocol, NetworkProtocol, Relayer, SyncSharedState, Synchronizer};
use ckb_traits::chain_provider::ChainProvider;
use ckb_types::prelude::*;
use ckb_verification::{BlockVerifier, Verifier};
use std::sync::Arc;

const SECP256K1_BLAKE160_SIGHASH_ALL_ARG_LEN: usize = 20;

pub fn run(args: RunArgs, version: Version) -> Result<(), ExitCode> {
    deadlock_detection();

    let (shared, table) = SharedBuilder::with_db_config(&args.config.db)
        .consensus(args.consensus)
        .tx_pool_config(args.config.tx_pool)
        .store_config(args.config.store)
        .build()
        .map_err(|err| {
            eprintln!("Run error: {:?}", err);
            ExitCode::Failure
        })?;

    // Verify genesis every time starting node
    verify_genesis(&shared)?;

    let notify = NotifyService::default().start(Some("notify"));
    let chain_service = ChainService::new(shared.clone(), table, notify.clone());
    let chain_controller = chain_service.start(Some("ChainService"));
    info_target!(
        crate::LOG_TARGET_MAIN,
        "chain genesis hash: {}",
        shared.genesis_hash()
    );

    let block_assembler_controller =
        match (args.config.rpc.miner_enable(), args.config.block_assembler) {
            (true, Some(block_assembler)) => {
                let check_lock_code_hash = |code_hash| -> Result<bool, ExitCode> {
                    let secp_cell_data =
                        Resource::bundled("specs/cells/secp256k1_blake160_sighash_all".to_string())
                            .get()
                            .map_err(|err| {
                                eprintln!(
                                    "Load specs/cells/secp256k1_blake160_sighash_all error: {:?}",
                                    err
                                );
                                ExitCode::Failure
                            })?;
                    let genesis_cellbase = &shared.consensus().genesis_block().transactions()[0];
                    Ok(genesis_cellbase
                        .outputs()
                        .into_iter()
                        .zip(genesis_cellbase.outputs_data().into_iter())
                        .any(|(output, data)| {
                            data.raw_data() == secp_cell_data.as_ref()
                                && output
                                    .type_()
                                    .to_opt()
                                    .map(|script| script.calc_script_hash())
                                    .as_ref()
                                    == Some(code_hash)
                        }))
                };
                if args.block_assembler_advanced
                    || (block_assembler.hash_type == ScriptHashType::Type
                        && block_assembler.args.len() == 1
                        && block_assembler.args[0].len() == SECP256K1_BLAKE160_SIGHASH_ALL_ARG_LEN
                        && check_lock_code_hash(&block_assembler.code_hash.pack())?)
                {
                    Some(
                        BlockAssembler::new(shared.clone(), block_assembler)
                            .start(Some("MinerAgent"), &notify),
                    )
                } else {
                    info_target!(
                    crate::LOG_TARGET_MAIN,
                    "Miner is disabled because block assmebler is not a recommended lock format. \
                     Edit ckb.toml or use `ckb run --ba-advanced` to use other lock scripts"
                );

                    None
                }
            }

            _ => {
                info_target!(
                    crate::LOG_TARGET_MAIN,
                    "Miner is disabled, edit ckb.toml to enable it"
                );

                None
            }
        };

    let sync_shared_state = Arc::new(SyncSharedState::new(shared.clone()));
    let network_state = Arc::new(
        NetworkState::from_config(args.config.network).expect("Init network state failed"),
    );
    let synchronizer = Synchronizer::new(chain_controller.clone(), Arc::clone(&sync_shared_state));

    let relayer = Relayer::new(chain_controller.clone(), Arc::clone(&sync_shared_state));
    let net_timer = NetTimeProtocol::default();
    let alert_signature_config = args.config.alert_signature.unwrap_or_default();
    let alert_notifier_config = args.config.alert_notifier.unwrap_or_default();
    let alert_relayer = AlertRelayer::new(
        version.to_string(),
        alert_notifier_config,
        alert_signature_config,
    );

    let alert_notifier = Arc::clone(alert_relayer.notifier());
    let alert_verifier = Arc::clone(alert_relayer.verifier());

    let synchronizer_clone = synchronizer.clone();
    let protocols = vec![
        CKBProtocol::new(
            "syn".to_string(),
            NetworkProtocol::SYNC.into(),
            &["1".to_string()][..],
            move || Box::new(synchronizer_clone.clone()),
            Arc::clone(&network_state),
        ),
        CKBProtocol::new(
            "rel".to_string(),
            NetworkProtocol::RELAY.into(),
            &["1".to_string()][..],
            move || Box::new(relayer.clone()),
            Arc::clone(&network_state),
        ),
        CKBProtocol::new(
            "tim".to_string(),
            NetworkProtocol::TIME.into(),
            &["1".to_string()][..],
            move || Box::new(net_timer.clone()),
            Arc::clone(&network_state),
        ),
        CKBProtocol::new(
            "alt".to_string(),
            NetworkProtocol::ALERT.into(),
            &["1".to_string()][..],
            move || Box::new(alert_relayer.clone()),
            Arc::clone(&network_state),
        ),
    ];
    let network_controller = NetworkService::new(
        Arc::clone(&network_state),
        protocols,
        shared.consensus().identify_name(),
        version.to_string(),
    )
    .start(version, Some("NetworkService"))
    .expect("Start network service failed");

    let builder = ServiceBuilder::new(&args.config.rpc)
        .enable_chain(shared.clone())
        .enable_pool(shared.clone(), sync_shared_state)
        .enable_miner(
            shared.clone(),
            network_controller.clone(),
            chain_controller.clone(),
            block_assembler_controller,
        )
        .enable_net(network_controller.clone())
        .enable_stats(shared.clone(), synchronizer, Arc::clone(&alert_notifier))
        .enable_experiment(shared.clone())
        .enable_integration_test(
            shared.clone(),
            network_controller.clone(),
            chain_controller.clone(),
        )
        .enable_alert(alert_verifier, alert_notifier, network_controller)
        .enable_indexer(&args.config.indexer, shared.clone());
    let io_handler = builder.build();

    let rpc_server = RpcServer::new(args.config.rpc, io_handler);

    wait_for_exit();

    info_target!(crate::LOG_TARGET_MAIN, "Finishing work, please wait...");

    rpc_server.close();
    info_target!(crate::LOG_TARGET_MAIN, "Jsonrpc shutdown");
    Ok(())
}

fn verify_genesis(shared: &Shared) -> Result<(), ExitCode> {
    let genesis = shared.consensus().genesis_block();
    BlockVerifier::new(shared.consensus())
        .verify(genesis)
        .map_err(|err| {
            eprintln!("genesis error: {}", err);
            ExitCode::Config
        })
}
