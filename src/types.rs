use serde::{Deserialize, Serialize};

/// Represents a flip recommendation from Coflnet
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flip {
    #[serde(rename = "itemName")]
    pub item_name: String,
    
    #[serde(rename = "startingBid")]
    pub starting_bid: u64,
    
    #[serde(rename = "target")]
    pub target: u64,
    
    #[serde(default)]
    pub finder: Option<String>,
    
    #[serde(rename = "profitPerc", default)]
    pub profit_perc: Option<f64>,

    /// Suggested buy time from COFL (grace-period end). Supports RFC3339 string,
    /// unix seconds, or unix milliseconds.
    #[serde(
        rename = "purchaseAt",
        default,
        deserialize_with = "deserialize_optional_timestamp_millis"
    )]
    pub purchase_at_ms: Option<i64>,
    
    #[serde(default, alias = "auctionUuid", alias = "auction_uuid", alias = "auctionId", alias = "id")]
    pub uuid: Option<String>,
}

fn deserialize_optional_timestamp_millis<'de, D>(
    deserializer: D,
) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(value) = value else {
        return Ok(None);
    };

    match value {
        serde_json::Value::Number(n) => {
            if let Some(v) = n.as_i64() {
                // Treat values above this as milliseconds; smaller values are unix seconds.
                if v > 1_000_000_000_000 {
                    Ok(Some(v))
                } else {
                    Ok(Some(v.saturating_mul(1000)))
                }
            } else {
                Err(D::Error::custom("invalid numeric purchaseAt"))
            }
        }
        serde_json::Value::String(s) => {
            if let Ok(v) = s.parse::<i64>() {
                // Treat values above this as milliseconds; smaller values are unix seconds.
                if v > 1_000_000_000_000 {
                    return Ok(Some(v));
                }
                return Ok(Some(v.saturating_mul(1000)));
            }
            chrono::DateTime::parse_from_rfc3339(&s)
                .map(|dt| Some(dt.timestamp_millis()))
                .map_err(|e| D::Error::custom(format!("invalid purchaseAt timestamp: {}", e)))
        }
        other => Err(D::Error::custom(format!(
            "invalid purchaseAt type: {:?}",
            other
        ))),
    }
}

/// Represents a bazaar flip recommendation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BazaarFlipRecommendation {
    #[serde(rename = "itemName", alias = "item", alias = "name")]
    pub item_name: String,
    
    #[serde(rename = "itemTag", default)]
    pub item_tag: Option<String>,
    
    #[serde(default)]
    pub amount: u64,
    
    #[serde(rename = "pricePerUnit", alias = "price", alias = "unitPrice", deserialize_with = "deserialize_price")]
    pub price_per_unit: f64,
    
    #[serde(rename = "totalPrice", default)]
    pub total_price: Option<f64>,
    
    #[serde(rename = "isBuyOrder", alias = "isBuy", default)]
    pub is_buy_order: bool,

    /// COFL sends "isSell" (the inverse of isBuyOrder). Captured here so callers can
    /// override `is_buy_order` when only `isSell` is present in the payload.
    #[serde(rename = "isSell", default)]
    pub is_sell: Option<bool>,
}

/// Deserialize a price value that may be either a JSON number or a comma-formatted string
/// (e.g. "1,544,775.5" or "333"). Strips commas before parsing.
fn deserialize_price<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    use serde::de::Error;
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(n) => n.as_f64().ok_or_else(|| D::Error::custom("invalid number")),
        serde_json::Value::String(s) => {
            let clean: String = s.chars().filter(|&c| c != ',').collect();
            clean.parse::<f64>().map_err(|e| D::Error::custom(format!("invalid price string: {}", e)))
        }
        other => Err(D::Error::custom(format!("expected number or string for price, got {:?}", other))),
    }
}

impl BazaarFlipRecommendation {
    pub fn calculate_total_price(&self) -> f64 {
        self.total_price.unwrap_or(self.price_per_unit * self.amount as f64)
    }

    /// Returns the effective buy-order flag, preferring `isSell` (negated) when
    /// `isBuyOrder`/`isBuy` was not explicitly sent by COFL (i.e. both default to false).
    /// Matches TypeScript parseBazaarFlipJson: `isBuyOrder = !data.isSell` when `isSell` present.
    pub fn effective_is_buy_order(&self) -> bool {
        if let Some(is_sell) = self.is_sell {
            !is_sell
        } else {
            self.is_buy_order
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Flip;

    #[test]
    fn test_flip_purchase_at_rfc3339_deserialization() {
        let value = serde_json::json!({
            "itemName": "Test Item",
            "startingBid": 1000,
            "target": 2000,
            "id": "abc",
            "purchaseAt": "2026-03-02T13:00:20Z"
        });
        let flip: Flip = serde_json::from_value(value).expect("flip should deserialize");
        assert_eq!(flip.purchase_at_ms, Some(1_772_456_420_000));
    }

    #[test]
    fn test_flip_purchase_at_unix_seconds_deserialization() {
        let value = serde_json::json!({
            "itemName": "Test Item",
            "startingBid": 1000,
            "target": 2000,
            "id": "abc",
            "purchaseAt": 1_772_456_420
        });
        let flip: Flip = serde_json::from_value(value).expect("flip should deserialize");
        assert_eq!(flip.purchase_at_ms, Some(1_772_456_420_000));
    }
}

/// Bot state enum
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotState {
    Startup,
    Idle,
    Purchasing,
    Bazaar,
    Selling,
    Claiming,
    GracePeriod,
    ClaimingPurchased,
    ClaimingSold,
    /// Step 2/4 of startup: cancelling old bazaar orders via /bz → Manage Orders
    ManagingOrders,
    /// Step 1/4 of startup: checking cookie status via /sbmenu
    CheckingCookie,
    /// Buying a booster cookie via /bz Booster Cookie → Buy Instantly
    BuyingCookie,
    /// Instaselling a dominant inventory item via /bz → Sell Instantly to free space
    InstaSelling,
}

impl BotState {
    /// Returns true if the bot can accept flip/trade commands.
    ///
    /// Matches TypeScript frikadellen-baf behaviour: `startup` and the active
    /// claiming states block commands (TypeScript: `if (bot.state)` blocks when
    /// state is non-null, which includes 'startup' and 'claiming').
    pub fn allows_commands(&self) -> bool {
        !matches!(self, BotState::Startup | BotState::ClaimingPurchased | BotState::ClaimingSold | BotState::ManagingOrders | BotState::CheckingCookie | BotState::BuyingCookie | BotState::InstaSelling)
    }
}

/// Command priority levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CommandPriority {
    Critical = 1,
    High = 2,
    Normal = 3,
    Low = 4,
}

/// Represents a queued command
#[derive(Debug, Clone)]
pub struct QueuedCommand {
    pub id: uuid::Uuid,
    pub priority: CommandPriority,
    pub command_type: CommandType,
    pub queued_at: std::time::Instant,
    pub interruptible: bool,
}

/// Types of commands
#[derive(Debug, Clone)]
pub enum CommandType {
    BazaarBuyOrder {
        item_name: String,
        item_tag: Option<String>,
        amount: u64,
        price_per_unit: f64,
    },
    BazaarSellOrder {
        item_name: String,
        item_tag: Option<String>,
        amount: u64,
        price_per_unit: f64,
    },
    PurchaseAuction {
        flip: Flip,
    },
    SendChat {
        message: String,
    },
    ClaimSoldItem,
    ClaimPurchasedItem,
    CheckCookie,
    DiscoverOrders,
    ExecuteOrders,
    SellToAuction {
        item_name: String,
        starting_bid: u64,
        duration_hours: u64,
        /// Mineflayer inventory slot (9-44) from COFL createAuction message
        item_slot: Option<u64>,
        /// ExtraAttributes.id from COFL for item identity verification
        item_id: Option<String>,
    },
    /// Trigger a full bazaar order management cycle (open /bz → Manage Orders → collect/cancel).
    ManageOrders,
    // Advanced commands matching TypeScript BAF.ts
    ClickSlot {
        slot: i16,
    },
    SwapProfile {
        profile_name: String,
    },
    AcceptTrade {
        player_name: String,
    },
}

/// Window types that can be opened
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowType {
    BazaarSearch,
    BazaarItemDetail,
    BazaarOrderCreation,
    BinAuctionView,
    ConfirmPurchase,
    ManageOrders,
    Storage,
    Other(String),
}

/// Represents an item in inventory or GUI
#[derive(Debug, Clone)]
pub struct ItemStack {
    pub name: String,
    pub count: u32,
    pub slot: usize,
    pub nbt: Option<serde_json::Value>,
}

impl ItemStack {
    pub fn skyblock_id(&self) -> Option<String> {
        self.nbt.as_ref()
            .and_then(|nbt| nbt.get("ExtraAttributes"))
            .and_then(|ea| ea.get("id"))
            .and_then(|id| id.as_str())
            .map(|s| s.to_string())
    }
}
