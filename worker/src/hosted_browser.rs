/// Only server-provisioned browsers may be adopted during entry preparation.
/// The Aliyun proxy is loopback-only and its path must match this binding;
/// arbitrary local Chrome debugging endpoints remain ineligible.
pub(crate) fn is_hosted_endpoint(endpoint: &str, browser_id: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(endpoint) else { return false };
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let Some(id) = browser_id.strip_prefix("ali-") else { return false };
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        && url.scheme() == "ws" && url.host_str() == Some("127.0.0.1")
        && url.port().is_some_and(|port| port != 0)
        && url.path() == format!("/v1/aliyun/cdp/{browser_id}")
        && url.query().is_none() && url.fragment().is_none()
        && endpoint == url.as_str()
}
