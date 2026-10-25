//! Reuse decoding only for identical bytes; filesystem authority stays with the caller.
use serde_json::Value;

#[derive(Default)]
pub(super) struct StateDecode {
    bytes: Vec<u8>,
    published: Option<String>,
    initialized: bool,
    #[cfg(test)]
    pub(super) parses: usize,
}

impl StateDecode {
    pub(super) fn published(&mut self, bytes: &[u8]) -> Option<Option<&str>> {
        if !self.initialized || self.bytes != bytes {
            #[cfg(test)]
            {
                self.parses += 1;
            }
            let state: Value = serde_json::from_slice(bytes).ok()?;
            self.published = state
                .get("linkScanPath")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            self.bytes.clear();
            self.bytes.extend_from_slice(bytes);
            self.initialized = false;
        }
        Some(self.published.as_deref())
    }
}
