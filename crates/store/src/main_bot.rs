//! The main bot ("메인 Bot", `getMainAgent` / `setMainAgent` in Grok Bot).
//!
//! Exactly one bot is the main bot whenever a bot exists. `state.json` stores the choice in
//! `mainBotId`; when it is unset (all data written before this feature, and new installs until the
//! user picks one) the first bot is the main bot, so existing files are not rewritten just to
//! record the default. The public snapshot mirrors the stored value (absent means the first bot),
//! so it stays the redacted projection of `state.json`; `web/state.ts` resolves it the same way.
//!
//! - Choosing the bot that already is the main bot is a no-op (no write, no revision).
//! - Choosing another bot keeps the sidebar stable: the previous main bot stays pinned (it was
//!   pinned by default only because it was main), and the new main bot is shown and pinned (as
//!   `setMainAgent` pins it in Grok Bot).
//! - Like other bot changes, it is refused while a run is active.
//! - Deleting the main bot makes the first remaining bot the main bot (`crate::bot_activity`).

use grok_bot_model::{DiskState, HttpResult};

use crate::State;

/// The main bot of `disk`: the chosen bot if it exists, otherwise the first bot.
pub fn resolve(disk: &DiskState) -> Option<&str> {
    disk.main_bot_id.as_deref().filter(|id| disk.bots.iter().any(|bot| bot.id == *id)).or_else(|| disk.bots.first().map(|bot| bot.id.as_str()))
}

impl State {
    /// The resolved main bot (see [`resolve`]).
    pub fn main_bot_id(&self) -> Option<&str> {
        resolve(&self.disk)
    }

    /// Make `id` the main bot (see the module documentation).
    pub fn set_main_bot(&mut self, id: &str) -> HttpResult<()> {
        self.assert_idle()?;
        let index = self.bot_index(id)?;
        let previous = self.main_bot_id().map(str::to_owned);
        if previous.as_deref() == Some(id) {
            return Ok(());
        }
        let disk = self.disk_mut();
        if let Some(old) = previous.and_then(|previous| disk.bots.iter_mut().find(|bot| bot.id == previous))
            && old.pinned.is_none()
        {
            old.pinned = Some(true);
        }
        let bot = &mut disk.bots[index];
        if bot.pinned == Some(false) {
            bot.pinned = None;
        }
        bot.hidden = None;
        disk.main_bot_id = Some(id.to_owned());
        Ok(())
    }
}
