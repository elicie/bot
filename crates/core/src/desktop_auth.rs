//! Opt-in per-launch credentials for the desktop HTTP adapter. Web hosting keeps its own policy.
use subtle::ConstantTimeEq;

/// A random secret held only by the native host and its application webview.
/// Deliberately does not implement Debug or Serialize.
pub struct DesktopToken(String);

impl DesktopToken {
    pub fn generate() -> anyhow::Result<Self> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("Cannot obtain desktop session randomness"))?;
        Ok(Self(hex::encode(bytes)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn matches(&self, value: &[u8]) -> bool {
        bool::from(self.0.as_bytes().ct_eq(value))
    }
}
