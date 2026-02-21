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
    
    #[serde(default, alias = "auctionUuid", alias = "auction_uuid", alias = "auctionId", alias = "id")]
    pub uuid: Option<String>,
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
    
    #[serde(rename = "pricePerUnit", alias = "price", alias = "unitPrice")]
    pub price_per_unit: f64,
    
    #[serde(rename = "totalPrice", default)]
    pub total_price: Option<f64>,
    
    #[serde(rename = "isBuyOrder", alias = "isBuy", default)]
    pub is_buy_order: bool,
}

impl BazaarFlipRecommendation {
    pub fn calculate_total_price(&self) -> f64 {
        self.total_price.unwrap_or(self.price_per_unit * self.amount as f64)
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
}

impl BotState {
    /// Returns true if the bot can accept flip/trade commands.
    ///
    /// Matches TypeScript frikadellen-baf behaviour: only the `startup` state
    /// blocks commands.  `gracePeriod` (and every other state) allows commands,
    /// exactly like TypeScript where the handlers only check
    /// `if (bot.state === 'startup')`.
    pub fn allows_commands(&self) -> bool {
        !matches!(self, BotState::Startup)
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
    },
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
    UploadInventory,
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
