use std::{sync::Arc, time::Duration};

use tokio::sync::Mutex;
use webb::evm::ethers::{
    abi::Detokenize,
    prelude::{builders::ContractCall, Middleware},
};
use webb_proposals::ResourceId;
use webb_relayer_handler_utils::{
    into_withdraw_error, CommandResponse, CommandStream, WithdrawStatus,
};
use webb_relayer_utils::metric::{self, Metrics};

pub mod fees;
/// Variable Anchor transaction relayer.
pub mod vanchor;

/// Submits a dry-run and then submits the actual transaction for an EVM transaction.
///
/// This is meant to be reused amongst all kinds of EVM transactions that the relayer sends.
/// The intention is that a dry-run call is made first to ensure that the transaction is valid
/// and then the actual transaction is submitted and its progress is monitored.
pub async fn handle_evm_tx<M, D>(
    call: ContractCall<M, D>,
    stream: CommandStream,
    chain_id: u64,
    metrics: Arc<Mutex<metric::Metrics>>,
    resource_id: ResourceId,
) -> Result<(), CommandResponse>
where
    M: Middleware,
    D: Detokenize,
{
    use CommandResponse::*;
    // Make a dry call, to make sure the transaction will go through successfully
    // to avoid wasting fees on invalid calls.
    call.call().await.map_err(|e| {
        tracing::error!("Error Client sent an invalid proof: {}", e);
        Withdraw(into_withdraw_error(e))
    })?;
    let _ = stream.send(Withdraw(WithdrawStatus::Valid)).await;
    tracing::debug!("Proof is valid");

    let pending = call.send().await.map_err(|e| {
        tracing::event!(
            target: webb_relayer_utils::probe::TARGET,
            tracing::Level::DEBUG,
            kind = %webb_relayer_utils::probe::Kind::PrivateTx,
            ty = "EVM",
            chain_id = %chain_id,
            errored = true,
            error = %e
        );
        Withdraw(into_withdraw_error(e))
    })?;

    let _ = stream.send(Withdraw(WithdrawStatus::Sent)).await;
    let tx_hash = *pending;
    tracing::event!(
        target: webb_relayer_utils::probe::TARGET,
        tracing::Level::DEBUG,
        kind = %webb_relayer_utils::probe::Kind::PrivateTx,
        ty = "EVM",
        chain_id = %chain_id,
        pending = true,
        %tx_hash,
    );
    let _ = stream
        .send(Withdraw(WithdrawStatus::Submitted { tx_hash }))
        .await;
    let receipt = pending
        .interval(Duration::from_millis(1000))
        .await
        .map_err(|e| {
            let reason = e.to_string();
            tracing::event!(
                target: webb_relayer_utils::probe::TARGET,
                tracing::Level::DEBUG,
                kind = %webb_relayer_utils::probe::Kind::PrivateTx,
                ty = "EVM",
                chain_id = %chain_id,
                errored = true,
                error = %reason
            );
            Withdraw(WithdrawStatus::Errored { reason, code: 4 })
        })?
        .ok_or(Withdraw(WithdrawStatus::DroppedFromMemPool))?;

    tracing::event!(
        target: webb_relayer_utils::probe::TARGET,
        tracing::Level::DEBUG,
        kind = %webb_relayer_utils::probe::Kind::PrivateTx,
        ty = "EVM",
        chain_id = %chain_id,
        finalized = true,
        tx_hash = %receipt.transaction_hash,
    );
    // gas spent by relayer on particular resource.
    let gas_price = receipt.gas_used.unwrap_or_default();
    let mut metrics = metrics.lock().await;
    let resource_metric = metrics
        .resource_metric_map
        .entry(resource_id)
        .or_insert_with(|| Metrics::register_resource_id_counters(resource_id));

    resource_metric
        .total_gas_spent
        .inc_by(gas_price.as_u64() as f64);
    drop(metrics);
    let _ = stream
        .send(Withdraw(WithdrawStatus::Finalized {
            tx_hash: receipt.transaction_hash,
        }))
        .await;
    Ok(())
}
