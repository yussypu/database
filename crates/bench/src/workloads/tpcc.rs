//! TPC-C-shape OLTP workload generator.
//!
//! Simplified TPC-C with one transaction type (new order) that demonstrates
//! the value of serializable isolation.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

/// TPC-C workload configuration.
#[derive(Debug, Clone)]
pub struct TpccConfig {
    /// Number of warehouses.
    pub warehouses: u32,
    /// Number of items.
    pub items: u32,
    /// Number of orders to run.
    pub orders_per_run: u64,
    /// Random seed for reproducibility.
    pub seed: u64,
}

impl Default for TpccConfig {
    fn default() -> Self {
        Self {
            warehouses: 1,
            items: 100,
            orders_per_run: 1000,
            seed: 0xDEADBEEF,
        }
    }
}

/// Key prefixes for TPC-C schema.
pub mod keys {
    /// Format: warehouse:{w_id}
    pub fn warehouse(w_id: u32) -> Vec<u8> {
        format!("warehouse:{:08}", w_id).into_bytes()
    }

    /// Format: item:{i_id}
    pub fn item(i_id: u32) -> Vec<u8> {
        format!("item:{:08}", i_id).into_bytes()
    }

    /// Format: stock:{w_id}:{i_id}
    pub fn stock(w_id: u32, i_id: u32) -> Vec<u8> {
        format!("stock:{:08}:{:08}", w_id, i_id).into_bytes()
    }

    /// Format: order:{w_id}:{o_id}
    pub fn order(w_id: u32, o_id: u64) -> Vec<u8> {
        format!("order:{:08}:{:016}", w_id, o_id).into_bytes()
    }

    /// Format: order_line:{w_id}:{o_id}:{ol_id}
    pub fn order_line(w_id: u32, o_id: u64, ol_id: u32) -> Vec<u8> {
        format!("order_line:{:08}:{:016}:{:04}", w_id, o_id, ol_id).into_bytes()
    }
}

/// TPC-C workload generator.
pub struct TpccGenerator {
    config: TpccConfig,
    rng: StdRng,
    next_order_id: Vec<u64>, // Per-warehouse order counter
}

impl TpccGenerator {
    /// Creates a new TPC-C generator with the given configuration.
    pub fn new(config: TpccConfig) -> Self {
        let rng = StdRng::seed_from_u64(config.seed);
        let next_order_id = vec![1; config.warehouses as usize];

        Self {
            config,
            rng,
            next_order_id,
        }
    }

    /// Returns an iterator over the initial load data.
    pub fn load_data(&self) -> impl Iterator<Item = (Vec<u8>, Vec<u8>)> + '_ {
        let warehouse_iter = (1..=self.config.warehouses).map(move |w_id| {
            let key = keys::warehouse(w_id);
            // Initial balance: 300000 cents = $3000
            let value = WarehouseValue { balance: 300000 }.encode();
            (key, value)
        });

        let item_iter = (1..=self.config.items).map(move |i_id| {
            let key = keys::item(i_id);
            // Random price between $1 and $100
            let mut rng = StdRng::seed_from_u64(self.config.seed.wrapping_add(i_id as u64));
            let price = rng.gen_range(100..10001) as u32; // cents
            let value = ItemValue { price }.encode();
            (key, value)
        });

        let stock_iter = (1..=self.config.warehouses).flat_map(move |w_id| {
            (1..=self.config.items).map(move |i_id| {
                let key = keys::stock(w_id, i_id);
                // Initial stock: 100 units
                let value = StockValue { quantity: 100 }.encode();
                (key, value)
            })
        });

        warehouse_iter.chain(item_iter).chain(stock_iter)
    }

    /// Generates a new order transaction.
    pub fn new_order(&mut self) -> NewOrderTxn {
        // Pick a random warehouse
        let w_id = self.rng.gen_range(1..=self.config.warehouses);

        // Get and increment order ID
        let w_idx = (w_id - 1) as usize;
        let o_id = self.next_order_id[w_idx];
        self.next_order_id[w_idx] += 1;

        // Pick 5-10 random items
        let item_count = self.rng.gen_range(5..=10);
        let mut items = Vec::with_capacity(item_count);

        for _ in 0..item_count {
            let i_id = self.rng.gen_range(1..=self.config.items);
            let quantity = self.rng.gen_range(1..=10);
            items.push(OrderItem { i_id, quantity });
        }

        NewOrderTxn { w_id, o_id, items }
    }
}

/// A new order transaction.
#[derive(Debug, Clone)]
pub struct NewOrderTxn {
    /// Warehouse ID.
    pub w_id: u32,
    /// Order ID.
    pub o_id: u64,
    /// Items in the order.
    pub items: Vec<OrderItem>,
}

/// An item in an order.
#[derive(Debug, Clone)]
pub struct OrderItem {
    /// Item ID.
    pub i_id: u32,
    /// Quantity ordered.
    pub quantity: u32,
}

/// Warehouse value encoding.
#[derive(Debug, Clone)]
pub struct WarehouseValue {
    pub balance: i64, // cents
}

impl WarehouseValue {
    pub fn encode(&self) -> Vec<u8> {
        self.balance.to_le_bytes().to_vec()
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() >= 8 {
            let balance = i64::from_le_bytes(bytes[..8].try_into().ok()?);
            Some(Self { balance })
        } else {
            None
        }
    }
}

/// Item value encoding.
#[derive(Debug, Clone)]
pub struct ItemValue {
    pub price: u32, // cents
}

impl ItemValue {
    pub fn encode(&self) -> Vec<u8> {
        self.price.to_le_bytes().to_vec()
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() >= 4 {
            let price = u32::from_le_bytes(bytes[..4].try_into().ok()?);
            Some(Self { price })
        } else {
            None
        }
    }
}

/// Stock value encoding.
#[derive(Debug, Clone)]
pub struct StockValue {
    pub quantity: i32, // can go negative if SSI fails
}

impl StockValue {
    pub fn encode(&self) -> Vec<u8> {
        self.quantity.to_le_bytes().to_vec()
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() >= 4 {
            let quantity = i32::from_le_bytes(bytes[..4].try_into().ok()?);
            Some(Self { quantity })
        } else {
            None
        }
    }
}

/// Order value encoding.
#[derive(Debug, Clone)]
pub struct OrderValue {
    pub item_count: u32,
    pub total: i64, // cents
}

impl OrderValue {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(12);
        bytes.extend_from_slice(&self.item_count.to_le_bytes());
        bytes.extend_from_slice(&self.total.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() >= 12 {
            let item_count = u32::from_le_bytes(bytes[..4].try_into().ok()?);
            let total = i64::from_le_bytes(bytes[4..12].try_into().ok()?);
            Some(Self { item_count, total })
        } else {
            None
        }
    }
}

/// Order line value encoding.
#[derive(Debug, Clone)]
pub struct OrderLineValue {
    pub i_id: u32,
    pub quantity: u32,
}

impl OrderLineValue {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8);
        bytes.extend_from_slice(&self.i_id.to_le_bytes());
        bytes.extend_from_slice(&self.quantity.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() >= 8 {
            let i_id = u32::from_le_bytes(bytes[..4].try_into().ok()?);
            let quantity = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
            Some(Self { i_id, quantity })
        } else {
            None
        }
    }
}

/// Executes a new order transaction against a backend.
///
/// Returns the total order amount and the list of stock decrements.
pub fn execute_new_order<B: crate::backends::Backend>(
    backend: &B,
    txn_data: &NewOrderTxn,
) -> Result<ExecutionResult, crate::backends::Error> {
    use crate::backends::BackendTxn;

    let mut txn = backend.begin()?;
    let mut total: i64 = 0;
    let mut stock_decrements: Vec<(u32, u32)> = Vec::new(); // (i_id, quantity)

    // For each item in the order
    for item in &txn_data.items {
        // Read item price
        let item_key = keys::item(item.i_id);
        let item_data = txn
            .get(&item_key)?
            .ok_or_else(|| crate::backends::Error::Other("item not found".to_string()))?;
        let item_val = ItemValue::decode(&item_data)
            .ok_or_else(|| crate::backends::Error::Other("invalid item data".to_string()))?;

        // Read stock, decrement, write back
        let stock_key = keys::stock(txn_data.w_id, item.i_id);
        let stock_data = txn
            .get(&stock_key)?
            .ok_or_else(|| crate::backends::Error::Other("stock not found".to_string()))?;
        let mut stock_val = StockValue::decode(&stock_data)
            .ok_or_else(|| crate::backends::Error::Other("invalid stock data".to_string()))?;

        stock_val.quantity -= item.quantity as i32;
        txn.put(&stock_key, &stock_val.encode())?;

        // Track decrement
        stock_decrements.push((item.i_id, item.quantity));

        // Add to total
        total += (item_val.price as i64) * (item.quantity as i64);
    }

    // Read warehouse balance, increment by total, write back
    let warehouse_key = keys::warehouse(txn_data.w_id);
    let warehouse_data = txn
        .get(&warehouse_key)?
        .ok_or_else(|| crate::backends::Error::Other("warehouse not found".to_string()))?;
    let mut warehouse_val = WarehouseValue::decode(&warehouse_data)
        .ok_or_else(|| crate::backends::Error::Other("invalid warehouse data".to_string()))?;

    warehouse_val.balance += total;
    txn.put(&warehouse_key, &warehouse_val.encode())?;

    // Insert order
    let order_key = keys::order(txn_data.w_id, txn_data.o_id);
    let order_val = OrderValue {
        item_count: txn_data.items.len() as u32,
        total,
    };
    txn.put(&order_key, &order_val.encode())?;

    // Insert order lines
    for (ol_id, item) in txn_data.items.iter().enumerate() {
        let ol_key = keys::order_line(txn_data.w_id, txn_data.o_id, ol_id as u32);
        let ol_val = OrderLineValue {
            i_id: item.i_id,
            quantity: item.quantity,
        };
        txn.put(&ol_key, &ol_val.encode())?;
    }

    // Commit
    let outcome = txn.commit()?;

    Ok(ExecutionResult {
        success: outcome.success,
        aborted_for_conflict: outcome.aborted_for_conflict,
        total,
        stock_decrements,
    })
}

/// Result of executing a new order transaction.
#[derive(Debug)]
pub struct ExecutionResult {
    pub success: bool,
    pub aborted_for_conflict: bool,
    pub total: i64,
    pub stock_decrements: Vec<(u32, u32)>, // (i_id, quantity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{Backend, BackendTxn, CrackeddbBackend};
    use tempfile::TempDir;

    #[test]
    fn tpcc_new_order_invariant_holds_serial() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("db");

        let config = TpccConfig {
            warehouses: 1,
            items: 100,
            orders_per_run: 1000,
            seed: 42,
        };

        let mut gen = TpccGenerator::new(config.clone());

        // Load initial data
        {
            let db = CrackeddbBackend::open(&path).unwrap();
            let mut txn = db.begin().unwrap();
            for (key, value) in gen.load_data() {
                txn.put(&key, &value).unwrap();
            }
            txn.commit().unwrap();
            db.close().unwrap();
        }

        // Run 1000 new-order transactions serially
        let db = CrackeddbBackend::open(&path).unwrap();

        let mut total_stock_decrements: u64 = 0;
        let mut total_order_line_quantities: u64 = 0;

        for _ in 0..config.orders_per_run {
            let txn_data = gen.new_order();
            let result = execute_new_order(&db, &txn_data).unwrap();

            if result.success {
                for (_, qty) in &result.stock_decrements {
                    total_stock_decrements += *qty as u64;
                }
            }
        }

        // Read all order lines and sum quantities
        {
            let mut txn = db.begin().unwrap();
            // Scan all order lines
            let results = txn
                .scan(
                    std::ops::Bound::Included(b"order_line:".as_ref()),
                    std::ops::Bound::Excluded(b"order_line:\xff".as_ref()),
                )
                .unwrap();

            for (_, value) in results {
                if let Some(ol) = OrderLineValue::decode(&value) {
                    total_order_line_quantities += ol.quantity as u64;
                }
            }
            txn.rollback().unwrap();
        }

        db.close().unwrap();

        // Invariant: sum of stock decrements == sum of order line quantities
        assert_eq!(
            total_stock_decrements, total_order_line_quantities,
            "stock decrements ({}) should equal order line quantities ({})",
            total_stock_decrements, total_order_line_quantities
        );
    }

    #[test]
    fn tpcc_load_data_generates_expected_keys() {
        let config = TpccConfig {
            warehouses: 2,
            items: 10,
            ..Default::default()
        };

        let gen = TpccGenerator::new(config);
        let data: Vec<_> = gen.load_data().collect();

        // Should have: 2 warehouses + 10 items + 2*10 stock entries = 32
        assert_eq!(data.len(), 2 + 10 + 20);
    }
}
