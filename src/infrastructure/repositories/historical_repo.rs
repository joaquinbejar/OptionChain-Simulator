use std::sync::Arc;
use optionstratlib::Positive;
use optionstratlib::utils::TimeFrame;
use crate::infrastructure::clickhouse::{ClickHouseClient, HistoricalDataRepository};

/// Implementation of HistoricalDataRepository using ClickHouse
pub struct ClickHouseHistoricalRepository {
    /// The underlying ClickHouse client
    client: Arc<ClickHouseClient>,
}

impl ClickHouseHistoricalRepository {
    /// Creates a new ClickHouseHistoricalRepository with the given client
    pub fn new(client: Arc<ClickHouseClient>) -> Self {
        Self { client }
    }
}

impl HistoricalDataRepository for ClickHouseHistoricalRepository {
    fn get_historical_prices(
        &self,
        symbol: &str,
        timeframe: &TimeFrame,
        start_date: &chrono::DateTime<chrono::Utc>,
        end_date: &chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<Positive>, String> {
        // Use tokio to block on the async operation
        let mut runtime = tokio::runtime::Runtime::new()
            .map_err(|e| format!("Failed to create Tokio runtime: {}", e))?;

        runtime.block_on(async {
            self.client.fetch_historical_prices(symbol, timeframe, start_date, end_date).await
        })
    }

    fn list_available_symbols(&self) -> Result<Vec<String>, String> {
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|e| format!("Failed to create Tokio runtime: {}", e))?;

        runtime.block_on(async {
            let mut conn = self.client.pool.get_handle()
                .await
                .map_err(|e| format!("Failed to get ClickHouse connection: {}", e))?;

            let query = "SELECT DISTINCT symbol FROM ohlcv ORDER BY symbol";

            let block = conn.query(query)
                .fetch_all()
                .await
                .map_err(|e| format!("Failed to execute ClickHouse query: {}", e))?;

            let mut symbols = Vec::new();
            for row in block.rows() {
                let symbol: String = row.get("symbol")
                    .map_err(|e| format!("Failed to get 'symbol' from row: {}", e))?;
                symbols.push(symbol);
            }

            Ok(symbols)
        })
    }

    fn get_date_range_for_symbol(&self, symbol: &str) -> Result<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>), String> {
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|e| format!("Failed to create Tokio runtime: {}", e))?;

        runtime.block_on(async {
            let mut conn = self.client.pool.get_handle()
                .await
                .map_err(|e| format!("Failed to get ClickHouse connection: {}", e))?;

            let query = format!(
                "SELECT 
                    min(timestamp) as min_date, 
                    max(timestamp) as max_date 
                FROM ohlcv 
                WHERE symbol = '{}'",
                symbol
            );

            let block = conn.query(query)
                .fetch_all()
                .await
                .map_err(|e| format!("Failed to execute ClickHouse query: {}", e))?;

            if let Some(row) = block.rows().next() {
                let min_date: chrono::DateTime<chrono::Utc> = row.get("min_date")
                    .map_err(|e| format!("Failed to get 'min_date' from row: {}", e))?;

                let max_date: chrono::DateTime<chrono::Utc> = row.get("max_date")
                    .map_err(|e| format!("Failed to get 'max_date' from row: {}", e))?;

                Ok((min_date, max_date))
            } else {
                Err(format!("No date range found for symbol: {}", symbol))
            }
        })
    }
}