pub struct SmritiClient;

impl Default for SmritiClient {
    fn default() -> Self {
        Self
    }
}

impl SmritiClient {
    pub fn new() -> Self {
        Self
    }

    pub async fn subscribe(&self) -> crate::error::Result<()> {
        tracing::info!("smriti subscription not available; using on-demand parse fallback");
        Ok(())
    }
}
