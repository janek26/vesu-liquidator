use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, Result};
use bigdecimal::num_bigint::BigInt;
use bigdecimal::BigDecimal;
use starknet::{
    core::types::{Call, Felt},
    providers::{jsonrpc::HttpTransport, JsonRpcClient},
};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::interval;

use crate::{
    config::Config,
    services::oracle::LatestOraclePrices,
    storages::Storage,
    types::{
        account::StarknetAccount,
        position::{Position, PositionsMap},
    },
    utils::wait_for_tx,
};

pub struct MonitoringService {
    config: Config,
    rpc_client: Arc<JsonRpcClient<HttpTransport>>,
    account: StarknetAccount,
    positions_receiver: UnboundedReceiver<(u64, Position)>,
    positions: PositionsMap,
    latest_oracle_prices: LatestOraclePrices,
    storage: Box<dyn Storage>,
    check_positions_interval: Duration,
    min_profit: BigDecimal,
}

impl MonitoringService {
    pub fn new(
        config: Config,
        rpc_client: Arc<JsonRpcClient<HttpTransport>>,
        account: StarknetAccount,
        positions_receiver: UnboundedReceiver<(u64, Position)>,
        latest_oracle_prices: LatestOraclePrices,
        storage: Box<dyn Storage>,
        check_positions_interval: u64,
        min_profit: BigDecimal,
    ) -> MonitoringService {
        MonitoringService {
            config,
            rpc_client,
            account,
            positions_receiver,
            positions: PositionsMap::from_storage(storage.as_ref()),
            latest_oracle_prices,
            storage,
            check_positions_interval: Duration::from_secs(check_positions_interval),
            min_profit,
        }
    }

    /// Starts the monitoring service.
    pub async fn start(mut self) -> Result<()> {
        let mut update_interval = interval(self.check_positions_interval);

        loop {
            tokio::select! {
                // Monitor the positions every N seconds
                _ = update_interval.tick() => {
                    self.monitor_positions_liquidability().await?;
                }

                // Insert the new positions indexed by the IndexerService
                maybe_position = self.positions_receiver.recv() => {
                    match maybe_position {
                        Some((block_number, new_position)) => {
                            self.positions.0.write().await.insert(new_position.key(), new_position);
                            self.storage.save(self.positions.0.read().await.clone(), block_number).await?;
                        }
                        None => {
                            return Err(anyhow!("⛔ Monitoring stopped unexpectedly."));
                        }
                    }
                }
            }
        }
    }

    /// Update all monitored positions and check if it's worth to liquidate any.
    /// TODO: Check issue for multicall update:
    /// https://github.com/astraly-labs/vesu-liquidator/issues/12
    async fn monitor_positions_liquidability(&self) -> Result<()> {
        let monitored_positions = self.positions.0.read().await;
        if monitored_positions.is_empty() {
            return Ok(());
        }
        tracing::info!("[🔭 Monitoring] Checking if any position is liquidable...");
        for (_, position) in monitored_positions.iter() {
            if position.is_liquidable(&self.latest_oracle_prices).await {
                tracing::info!(
                    "[🔭 Monitoring] Liquidatable position found #{}!",
                    position.key()
                );
                let _profit_made = self.try_to_liquidate_position(position).await?;
            }
        }
        tracing::info!("[🔭 Monitoring] 🤨 They're good.. for now...");
        Ok(())
    }

    /// Check if a position is liquidable, computes the profitability and if it's worth it
    /// liquidate it.
    async fn try_to_liquidate_position(&self, position: &Position) -> Result<BigDecimal> {
        let (profit, txs) = self.compute_profitability(position).await?;
        if profit >= self.min_profit {
            tracing::info!(
                "[🔭 Monitoring] Trying to liquidate position for #{} {}!",
                profit,
                position.debt.name
            );
            let tx_hash_felt = self.account.execute_txs(&txs).await?;
            let tx_hash = tx_hash_felt.to_string();
            self.wait_for_tx_to_be_accepted(&tx_hash).await?;
            tracing::info!(
                "[🔭 Monitoring] ✅ Liquidated position #{}! (TX #{})",
                position.key(),
                tx_hash
            );
        } else {
            tracing::info!(
                "[🔭 Monitoring] Position is not worth liquidating (estimated profit: {}, minimum required: {}), skipping...",
                profit,
                self.min_profit
            );
        }
        Ok(profit)
    }

    /// Simulates the profit generated by liquidating a given position. Returns the profit
    /// and the transactions needed to liquidate the position.
    async fn compute_profitability(&self, position: &Position) -> Result<(BigDecimal, Vec<Call>)> {
        let (liquidable_amount_as_debt_asset, liquidable_amount_as_collateral_asset) = position
            .liquidable_amount(self.config.liquidation_mode, &self.latest_oracle_prices)
            .await?;

        let liquidation_factor = position
            .fetch_liquidation_factors(&self.config, self.rpc_client.clone())
            .await;

        let debt_to_liquidate = match self.config.liquidation_mode {
            crate::config::LiquidationMode::Full => BigDecimal::from(0),
            crate::config::LiquidationMode::Partial => {
                liquidable_amount_as_debt_asset.clone() * liquidation_factor.clone()
            }
        };
        let min_collateral_to_receive =
            liquidable_amount_as_collateral_asset * liquidation_factor.clone();
        let simulated_profit: BigDecimal =
            liquidable_amount_as_debt_asset.clone() * (1 - liquidation_factor.clone());
        let liquidation_txs = position
            .get_liquidation_txs(
                &self.account,
                self.config.liquidate_address,
                debt_to_liquidate,
                min_collateral_to_receive,
            )
            .await?;
        let execution_fees = self.account.estimate_fees_cost(&liquidation_txs).await?;
        let slippage = BigDecimal::new(BigInt::from(5), 2);
        let slippage_factor = BigDecimal::from(1) - slippage;

        Ok((
            (simulated_profit * slippage_factor) - execution_fees,
            liquidation_txs,
        ))
    }

    /// Waits for a TX to be accepted on-chain.
    pub async fn wait_for_tx_to_be_accepted(&self, tx_hash: &str) -> Result<()> {
        let tx_hash = Felt::from_hex(tx_hash)?;
        wait_for_tx(tx_hash, self.rpc_client.clone()).await?;
        Ok(())
    }
}
