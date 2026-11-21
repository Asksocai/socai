//! Only the configured Socai HTTPS origin may receive a device credential.
use anyhow::{Result, bail};
use reqwest::Url;

pub(super) fn connect_url(base: &str, browser_id: &str) -> Result<String> {
    if !browser_id.strip_prefix("ali-").is_some_and(|id| {
        id.len() == 32
            && id
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    }) {
        bail!("invalid hosted browser identifier");
    }
    let mut url = Url::parse(base).map_err(|_| anyhow::anyhow!("invalid hosted browser server"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("hosted browser gateway requires a private HTTPS connection");
    }
    url.set_path(&format!(
        "{}/v1/browser/sessions/{browser_id}/cdp",
        url.path().trim_end_matches('/')
    ));
    url.set_scheme("wss")
        .map_err(|_| anyhow::anyhow!("invalid hosted browser scheme"))?;
    Ok(url.into())
}

pub(super) fn matches(base: &str, websocket_url: &str) -> bool {
    let Ok(url) = Url::parse(websocket_url) else {
        return false;
    };
    let Some(browser_id) = url
        .path()
        .strip_suffix("/cdp")
        .and_then(|p| p.rsplit('/').next())
    else {
        return false;
    };
    connect_url(base, browser_id).ok().as_deref() == Some(url.as_str())
}
