use serde_json::{json, Value};

use crate::cdp::connection::Cdp;
use crate::cdp::session::PageSession;

/// Cancellation-safe ownership for short-lived parallel worker tabs.
pub struct OwnedPage(Option<std::sync::Arc<PageSession>>);

impl OwnedPage {
    pub(crate) fn new(page: PageSession) -> Self {
        Self(Some(std::sync::Arc::new(page)))
    }
    pub fn page(&self) -> std::sync::Arc<PageSession> {
        self.0.as_ref().expect("owned page").clone()
    }
    pub async fn close(mut self) {
        if let Some(page) = self.0.as_ref() {
            let _ = page.close_target().await;
        }
        self.0.take();
    }
}

impl Drop for OwnedPage {
    fn drop(&mut self) {
        if let Some(page) = self.0.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = page.close_target().await;
                });
            }
        }
    }
}

/// Owns a target from the moment Chrome returns its id until the fully
/// attached `PageSession` takes over. Cancellation during attachment drops
/// this guard and closes the otherwise orphaned tab.
struct TargetCreationGuard {
    cdp: Cdp,
    client: crate::cdp::raw_client::RawCdpClient,
    target_id: Option<String>,
}

impl TargetCreationGuard {
    fn disarm(&mut self) {
        self.target_id = None;
    }

    async fn close(mut self) {
        let Some(target_id) = self.target_id.take() else {
            return;
        };
        let _ = self
            .client
            .execute("Target.closeTarget", json!({ "targetId": target_id }))
            .await;
        self.cdp.unregister_owned_target(&target_id).await;
    }
}

impl Drop for TargetCreationGuard {
    fn drop(&mut self) {
        let Some(target_id) = self.target_id.take() else {
            return;
        };
        let client = self.client.clone();
        let cdp = self.cdp.clone();
        tokio::spawn(async move {
            let _ = client
                .execute("Target.closeTarget", json!({ "targetId": target_id }))
                .await;
            cdp.unregister_owned_target(&target_id).await;
        });
    }
}

/// Thin page factory over one remote-debugging endpoint. Higher-level runtime
/// code decides whether a page belongs to a tool session, an agent run, or a
/// debug command.
pub struct PageSessionManager {
    cdp: Cdp,
}

impl PageSessionManager {
    pub fn new(cdp: Cdp) -> Self {
        Self { cdp }
    }

    /// Open a new socai-owned tab and control only that target. All target
    /// lifecycle is routed through the browser websocket (`Target.*`) so
    /// existing and managed Chrome share one code path. This deliberately
    /// avoids browser-wide CDP target discovery/auto-attach, so unrelated user
    /// tabs are not instrumented.
    pub async fn create_page(&self, start_url: &str) -> anyhow::Result<PageSession> {
        self.create_page_with_options(start_url, false).await
    }

    /// Create an owned tab without bringing it to the foreground. This is used
    /// for short-lived control contexts that must not steal focus from the site
    /// tab the user is watching.
    pub async fn create_background_page(&self, start_url: &str) -> anyhow::Result<PageSession> {
        self.create_page_with_options(start_url, true).await
    }

    /// Adopt a page already opened by the host in this dedicated browser.
    /// Only the hosted entry-preparation path uses this; ordinary local Chrome
    /// tools continue to create their own tabs. Never navigate or activate it.
    pub(crate) async fn attach_existing_page(
        &self,
        start_url: &str,
    ) -> anyhow::Result<Option<PageSession>> {
        let expected = reqwest::Url::parse(start_url)?;
        anyhow::ensure!(
            expected.scheme() == "https" && expected.host_str().is_some(),
            "Hosted page requires an HTTPS origin"
        );
        let (client, remote) = self
            .cdp
            .browser_client_with_mode()
            .await
            .ok_or_else(|| anyhow::anyhow!("CDP browser websocket is not connected"))?;
        let inventory = client.execute("Target.getTargets", json!({})).await?;
        let targets = inventory
            .get("targetInfos")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("Target.getTargets missing targetInfos"))?;
        for target in targets {
            if target.get("type").and_then(Value::as_str) != Some("page") {
                continue;
            }
            let Some(url) = target
                .get("url")
                .and_then(Value::as_str)
                .and_then(|url| reqwest::Url::parse(url).ok())
            else {
                continue;
            };
            if url.origin() != expected.origin() {
                continue;
            }
            let target_id = target
                .get("targetId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("Hosted page missing targetId"))?;
            let attached = client
                .execute(
                    "Target.attachToTarget",
                    json!({"targetId":target_id,"flatten":true}),
                )
                .await?;
            let session_id = attached
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("Target.attachToTarget missing sessionId"))?;
            let page = PageSession::attached(
                target_id.to_owned(),
                client.clone(),
                session_id.to_owned(),
                self.cdp.clone(),
                remote,
                false,
            );
            // The page may have redirected while attachment was in flight.
            // Re-read its actual URL before admitting it into the runtime.
            let info = page.page_info().await;
            let same_origin = info
                .as_ref()
                .ok()
                .and_then(|v| v.get("url"))
                .and_then(Value::as_str)
                .and_then(|url| reqwest::Url::parse(url).ok())
                .is_some_and(|url| url.origin() == expected.origin());
            if !same_origin {
                let _ = client
                    .execute("Target.detachFromTarget", json!({"sessionId":session_id}))
                    .await;
                anyhow::bail!("Hosted page changed origin during preparation");
            }
            self.cdp.register_owned_target(target_id.to_owned()).await;
            return Ok(Some(page));
        }
        Ok(None)
    }

    async fn create_page_with_options(
        &self,
        start_url: &str,
        background: bool,
    ) -> anyhow::Result<PageSession> {
        // Client and browser mode come from one locked read: the page is
        // labelled with the browser it is actually created in, even if the
        // connection is replaced while the target commands below are in flight.
        let (browser_client, remote_browser) = self
            .cdp
            .browser_client_with_mode()
            .await
            .ok_or_else(|| anyhow::anyhow!("CDP browser websocket is not connected"))?;
        self.create_page_via_browser_ws(browser_client, remote_browser, start_url, background)
            .await
    }

    async fn create_page_via_browser_ws(
        &self,
        browser_client: crate::cdp::raw_client::RawCdpClient,
        remote_browser: bool,
        start_url: &str,
        background: bool,
    ) -> anyhow::Result<PageSession> {
        let action = if blank_or_start_url(start_url) != "about:blank" {
            Some(super::pacing::BrowserAction::begin(start_url).await?)
        } else {
            None
        };
        let mut create_params = json!({ "url": blank_or_start_url(start_url) });
        if background {
            create_params["background"] = Value::Bool(true);
        }
        let created = browser_client
            .execute("Target.createTarget", create_params)
            .await?;
        let target_id = created
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Target.createTarget missing targetId"))?
            .to_string();
        let mut target_guard = TargetCreationGuard {
            cdp: self.cdp.clone(),
            client: browser_client.clone(),
            target_id: Some(target_id.clone()),
        };

        let attached = match browser_client
            .execute(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await
        {
            Ok(attached) => attached,
            Err(err) => {
                target_guard.close().await;
                return Err(err);
            }
        };
        let session_id = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Target.attachToTarget missing sessionId"))?
            .to_string();

        self.cdp.register_owned_target(target_id.clone()).await;
        let page = PageSession::attached(
            target_id,
            browser_client,
            session_id,
            self.cdp.clone(),
            remote_browser,
            background,
        );
        if let Some(action) = action {
            if action.speed != super::BrowserActionSpeed::Instant
                && !page.wait_for_load_state("domcontentloaded", 15.0).await?
            {
                anyhow::bail!("new browser page did not become DOM-ready after 15s");
            }
            action.finish().await;
        }
        target_guard.disarm();
        Ok(page)
    }

    /// Close a page target by target id. This is stronger than consuming a
    /// `PageSession`: cancellation paths may only have a task snapshot and an
    /// id, or the page may still be held by tool `Arc`s.
    pub async fn close_target(&self, target_id: &str) -> anyhow::Result<bool> {
        let target_id = target_id.trim();
        if target_id.is_empty() {
            return Ok(false);
        }
        let browser_client = self
            .cdp
            .browser_client()
            .await
            .ok_or_else(|| anyhow::anyhow!("CDP browser websocket is not connected"))?;
        browser_client
            .execute("Target.closeTarget", json!({ "targetId": target_id }))
            .await?;
        self.cdp.unregister_owned_target(target_id).await;
        Ok(true)
    }
}

fn blank_or_start_url(start_url: &str) -> &str {
    let start_url = start_url.trim();
    if start_url.is_empty() {
        "about:blank"
    } else {
        start_url
    }
}
