//! Estimate gas needed implementation

use super::{Call, LoadPendingBlock};
use crate::{AsEthApiError, FromEthApiError, IntoEthApiError};
use alloy_primitives::{TxKind, U256};
use alloy_rpc_types_eth::{state::StateOverride, transaction::TransactionRequest, BlockId};
use futures::Future;
use reth_chainspec::MIN_TRANSACTION_GAS;
use reth_errors::ProviderError;
use reth_evm::{Database, EvmEnvFor, TransactionEnv, TxEnvFor};
use reth_revm::{database::StateProviderDatabase, db::CacheDB};
use reth_rpc_eth_types::{
    error::api::FromEvmHalt,
    revm_utils::{apply_state_overrides, caller_gas_allowance},
    EthApiError, RevertError, RpcInvalidTransactionError,
};
use reth_rpc_server_types::constants::gas_oracle::{CALL_STIPEND_GAS, ESTIMATE_GAS_ERROR_RATIO};
use reth_storage_api::StateProvider;
use revm::context_interface::{result::ExecutionResult, Transaction};
use tracing::trace;

/// Gas execution estimates
pub trait EstimateCall: Call {
    /// Estimates the gas usage of the `request` with the state.
    ///
    /// This will execute the [`TransactionRequest`] and find the best gas limit via binary search.
    ///
    /// ## EVM settings
    ///
    /// This modifies certain EVM settings to mirror geth's `SkipAccountChecks` when transacting requests, see also: <https://github.com/ethereum/go-ethereum/blob/380688c636a654becc8f114438c2a5d93d2db032/core/state_transition.go#L145-L148>:
    ///
    ///  - `disable_eip3607` is set to `true`
    ///  - `disable_base_fee` is set to `true`
    ///  - `nonce` is set to `None`
    fn estimate_gas_with<S>(
        &self,
        mut evm_env: EvmEnvFor<Self::Evm>,
        mut request: TransactionRequest,
        state: S,
        state_override: Option<StateOverride>,
    ) -> Result<U256, Self::Error>
    where
        S: StateProvider,
    {
        // Disabled because eth_estimateGas is sometimes used with eoa senders
        // See <https://github.com/paradigmxyz/reth/issues/1959>
        evm_env.cfg_env.disable_eip3607 = true;

        // The basefee should be ignored for eth_estimateGas and similar
        // See:
        // <https://github.com/ethereum/go-ethereum/blob/ee8e83fa5f6cb261dad2ed0a7bbcde4930c41e6c/internal/ethapi/api.go#L985>
        evm_env.cfg_env.disable_base_fee = true;

        // set nonce to None so that the correct nonce is chosen by the EVM
        request.nonce = None;

        // Keep a copy of gas related request values
        let tx_request_gas_limit = request.gas;
        let tx_request_gas_price = request.gas_price;
        // the gas limit of the corresponding block
        let block_env_gas_limit = evm_env.block_env.gas_limit;

        // Determine the highest possible gas limit, considering both the request's specified limit
        // and the block's limit.
        let mut highest_gas_limit = tx_request_gas_limit
            .map(|mut tx_gas_limit| {
                if block_env_gas_limit < tx_gas_limit {
                    // requested gas limit is higher than the allowed gas limit, capping
                    tx_gas_limit = block_env_gas_limit;
                }
                tx_gas_limit
            })
            .unwrap_or(block_env_gas_limit);

        // Configure the evm env
        let mut db = CacheDB::new(StateProviderDatabase::new(state));
        let mut tx_env = self.create_txn_env(&evm_env, request, &mut db)?;

        // Apply any state overrides if specified.
        if let Some(state_override) = state_override {
            apply_state_overrides(state_override, &mut db).map_err(Self::Error::from_eth_err)?;
        }

        // Optimize for simple transfer transactions, potentially reducing the gas estimate.
        if tx_env.input().is_empty() {
            if let TxKind::Call(to) = tx_env.kind() {
                if let Ok(code) = db.db.account_code(&to) {
                    let no_code_callee = code.map(|code| code.is_empty()).unwrap_or(true);
                    if no_code_callee {
                        // If the tx is a simple transfer (call to an account with no code) we can
                        // shortcircuit. But simply returning
                        // `MIN_TRANSACTION_GAS` is dangerous because there might be additional
                        // field combos that bump the price up, so we try executing the function
                        // with the minimum gas limit to make sure.
                        let mut tx_env = tx_env.clone();
                        tx_env.set_gas_limit(MIN_TRANSACTION_GAS);
                        if let Ok(res) = self.transact(&mut db, evm_env.clone(), tx_env) {
                            if res.result.is_success() {
                                return Ok(U256::from(MIN_TRANSACTION_GAS))
                            }
                        }
                    }
                }
            }
        }

        // Check funds of the sender (only useful to check if transaction gas price is more than 0).
        //
        // The caller allowance is check by doing `(account.balance - tx.value) / tx.gas_price`
        if tx_env.gas_price() > 0 {
            // cap the highest gas limit by max gas caller can afford with given gas price
            highest_gas_limit = highest_gas_limit
                .min(caller_gas_allowance(&mut db, &tx_env).map_err(Self::Error::from_eth_err)?);
        }

        // If the provided gas limit is less than computed cap, use that
        tx_env.set_gas_limit(tx_env.gas_limit().min(highest_gas_limit));

        trace!(target: "rpc::eth::estimate", ?evm_env, ?tx_env, "Starting gas estimation");

        // Execute the transaction with the highest possible gas limit.
        let mut res = match self.transact(&mut db, evm_env.clone(), tx_env.clone()) {
            // Handle the exceptional case where the transaction initialization uses too much
            // gas. If the gas price or gas limit was specified in the request,
            // retry the transaction with the block's gas limit to determine if
            // the failure was due to insufficient gas.
            Err(err)
                if err.is_gas_too_high() &&
                    (tx_request_gas_limit.is_some() || tx_request_gas_price.is_some()) =>
            {
                return Err(self.map_out_of_gas_err(block_env_gas_limit, evm_env, tx_env, &mut db))
            }
            Err(err) if err.is_gas_too_low() => {
                // This failed because the configured gas cost of the tx was lower than what
                // actually consumed by the tx This can happen if the
                // request provided fee values manually and the resulting gas cost exceeds the
                // sender's allowance, so we return the appropriate error here
                return Err(RpcInvalidTransactionError::GasRequiredExceedsAllowance {
                    gas_limit: tx_env.gas_limit(),
                }
                .into_eth_err())
            }
            // Propagate other results (successful or other errors).
            ethres => ethres?,
        };

        let gas_refund = match res.result {
            ExecutionResult::Success { gas_refunded, .. } => gas_refunded,
            ExecutionResult::Halt { reason, .. } => {
                // here we don't check for invalid opcode because already executed with highest gas
                // limit
                return Err(Self::Error::from_evm_halt(reason, tx_env.gas_limit()))
            }
            ExecutionResult::Revert { output, .. } => {
                // if price or limit was included in the request then we can execute the request
                // again with the block's gas limit to check if revert is gas related or not
                return if tx_request_gas_limit.is_some() || tx_request_gas_price.is_some() {
                    Err(self.map_out_of_gas_err(block_env_gas_limit, evm_env, tx_env, &mut db))
                } else {
                    // the transaction did revert
                    Err(RpcInvalidTransactionError::Revert(RevertError::new(output)).into_eth_err())
                }
            }
        };

        // At this point we know the call succeeded but want to find the _best_ (lowest) gas the
        // transaction succeeds with. We find this by doing a binary search over the possible range.

        // we know the tx succeeded with the configured gas limit, so we can use that as the
        // highest, in case we applied a gas cap due to caller allowance above
        highest_gas_limit = tx_env.gas_limit();

        // NOTE: this is the gas the transaction used, which is less than the
        // transaction requires to succeed.
        let mut gas_used = res.result.gas_used();
        // the lowest value is capped by the gas used by the unconstrained transaction
        let mut lowest_gas_limit = gas_used.saturating_sub(1);

        // As stated in Geth, there is a good chance that the transaction will pass if we set the
        // gas limit to the execution gas used plus the gas refund, so we check this first
        // <https://github.com/ethereum/go-ethereum/blob/a5a4fa7032bb248f5a7c40f4e8df2b131c4186a4/eth/gasestimator/gasestimator.go#L135
        //
        // Calculate the optimistic gas limit by adding gas used and gas refund,
        // then applying a 64/63 multiplier to account for gas forwarding rules.
        let optimistic_gas_limit = (gas_used + gas_refund + CALL_STIPEND_GAS) * 64 / 63;
        if optimistic_gas_limit < highest_gas_limit {
            // Set the transaction's gas limit to the calculated optimistic gas limit.
            tx_env.set_gas_limit(optimistic_gas_limit);
            // Re-execute the transaction with the new gas limit and update the result and
            // environment.
            res = self.transact(&mut db, evm_env.clone(), tx_env.clone())?;
            // Update the gas used based on the new result.
            gas_used = res.result.gas_used();
            // Update the gas limit estimates (highest and lowest) based on the execution result.
            update_estimated_gas_range(
                res.result,
                optimistic_gas_limit,
                &mut highest_gas_limit,
                &mut lowest_gas_limit,
            )?;
        };

        // Pick a point that's close to the estimated gas
        let mut mid_gas_limit = std::cmp::min(
            gas_used * 3,
            ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64,
        );

        trace!(target: "rpc::eth::estimate", ?evm_env, ?tx_env, ?highest_gas_limit, ?lowest_gas_limit, ?mid_gas_limit, "Starting binary search for gas");

        // Binary search narrows the range to find the minimum gas limit needed for the transaction
        // to succeed.
        while lowest_gas_limit + 1 < highest_gas_limit {
            // An estimation error is allowed once the current gas limit range used in the binary
            // search is small enough (less than 1.5% of the highest gas limit)
            // <https://github.com/ethereum/go-ethereum/blob/a5a4fa7032bb248f5a7c40f4e8df2b131c4186a4/eth/gasestimator/gasestimator.go#L152
            if (highest_gas_limit - lowest_gas_limit) as f64 / (highest_gas_limit as f64) <
                ESTIMATE_GAS_ERROR_RATIO
            {
                break
            };

            tx_env.set_gas_limit(mid_gas_limit);

            // Execute transaction and handle potential gas errors, adjusting limits accordingly.
            match self.transact(&mut db, evm_env.clone(), tx_env.clone()) {
                Err(err) if err.is_gas_too_high() => {
                    // Decrease the highest gas limit if gas is too high
                    highest_gas_limit = mid_gas_limit;
                }
                Err(err) if err.is_gas_too_low() => {
                    // Increase the lowest gas limit if gas is too low
                    lowest_gas_limit = mid_gas_limit;
                }
                // Handle other cases, including successful transactions.
                ethres => {
                    // Unpack the result and environment if the transaction was successful.
                    res = ethres?;
                    // Update the estimated gas range based on the transaction result.
                    update_estimated_gas_range(
                        res.result,
                        mid_gas_limit,
                        &mut highest_gas_limit,
                        &mut lowest_gas_limit,
                    )?;
                }
            }

            // New midpoint
            mid_gas_limit = ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64;
        }

        Ok(U256::from(highest_gas_limit))
    }

    /// Estimate gas needed for execution of the `request` at the [`BlockId`].
    fn estimate_gas_at(
        &self,
        request: TransactionRequest,
        at: BlockId,
        state_override: Option<StateOverride>,
    ) -> impl Future<Output = Result<U256, Self::Error>> + Send
    where
        Self: LoadPendingBlock,
    {
        async move {
            let (evm_env, at) = self.evm_env_at(at).await?;

            self.spawn_blocking_io(move |this| {
                let state = this.state_at_block_id(at)?;
                EstimateCall::estimate_gas_with(&this, evm_env, request, state, state_override)
            })
            .await
        }
    }

    /// Executes the requests again after an out of gas error to check if the error is gas related
    /// or not
    #[inline]
    fn map_out_of_gas_err<DB>(
        &self,
        env_gas_limit: u64,
        evm_env: EvmEnvFor<Self::Evm>,
        mut tx_env: TxEnvFor<Self::Evm>,
        db: &mut DB,
    ) -> Self::Error
    where
        DB: Database<Error = ProviderError>,
        EthApiError: From<DB::Error>,
    {
        let req_gas_limit = tx_env.gas_limit();
        tx_env.set_gas_limit(env_gas_limit);
        let res = match self.transact(db, evm_env, tx_env) {
            Ok(res) => res,
            Err(err) => return err,
        };
        match res.result {
            ExecutionResult::Success { .. } => {
                // transaction succeeded by manually increasing the gas limit to
                // highest, which means the caller lacks funds to pay for the tx
                RpcInvalidTransactionError::BasicOutOfGas(req_gas_limit).into_eth_err()
            }
            ExecutionResult::Revert { output, .. } => {
                // reverted again after bumping the limit
                RpcInvalidTransactionError::Revert(RevertError::new(output)).into_eth_err()
            }
            ExecutionResult::Halt { reason, .. } => {
                Self::Error::from_evm_halt(reason, req_gas_limit)
            }
        }
    }
}

/// Updates the highest and lowest gas limits for binary search based on the execution result.
///
/// This function refines the gas limit estimates used in a binary search to find the optimal
/// gas limit for a transaction. It adjusts the highest or lowest gas limits depending on
/// whether the execution succeeded, reverted, or halted due to specific reasons.
#[inline]
pub fn update_estimated_gas_range<Halt>(
    result: ExecutionResult<Halt>,
    tx_gas_limit: u64,
    highest_gas_limit: &mut u64,
    lowest_gas_limit: &mut u64,
) -> Result<(), EthApiError> {
    match result {
        ExecutionResult::Success { .. } => {
            // Cap the highest gas limit with the succeeding gas limit.
            *highest_gas_limit = tx_gas_limit;
        }
        ExecutionResult::Revert { .. } | ExecutionResult::Halt { .. } => {
            // We know that transaction succeeded with a higher gas limit before, so any failure
            // means that we need to increase it.
            //
            // We are ignoring all halts here, and not just OOG errors because there are cases when
            // non-OOG halt might flag insufficient gas limit as well.
            //
            // Common usage of invalid opcode in OpenZeppelin:
            // <https://github.com/OpenZeppelin/openzeppelin-contracts/blob/94697be8a3f0dfcd95dfb13ffbd39b5973f5c65d/contracts/metatx/ERC2771Forwarder.sol#L360-L367>
            *lowest_gas_limit = tx_gas_limit;
        }
    };

    Ok(())
}
