//! Bot-to-bot delivery data rules. Port of `server/bot-messages.ts`. CONTRACT (store agent).

use grok_bot_model::{BotDelivery, BotMessageInput, HttpResult};

pub const MAX_BOT_HOPS: u32 = 4;
pub const MAX_BOT_DELIVERIES: usize = 10000;
pub const MAX_BOT_MESSAGE_BYTES: usize = 160 * 1024;
pub const MAX_BOT_REPLY_CHARACTERS: usize = 200000;

pub fn validate_bot_message_input(input: BotMessageInput) -> HttpResult<BotMessageInput> {
    let _ = input;
    todo!("store agent")
}

/// The durable receipt JSON returned to the sending model.
pub fn bot_delivery_receipt(delivery: &BotDelivery) -> serde_json::Value {
    let _ = delivery;
    todo!("store agent")
}

/// An existing delivery for the same source message, target, and text (idempotent retry).
pub fn existing_bot_delivery<'a>(deliveries: &'a [BotDelivery], source_message_id: &str, input: &BotMessageInput) -> Option<&'a BotDelivery> {
    let _ = (deliveries, source_message_id, input);
    todo!("store agent")
}
