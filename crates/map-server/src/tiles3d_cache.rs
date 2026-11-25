//! Read-through object cache; Map remains the authority for visibility and manifests.
use crate::AppState;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use std::{path::Path, sync::OnceLock, time::Duration};
const MAX: usize = 32 * 1024 * 1024;
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_millis(700))
            .timeout(Duration::from_secs(3))
            .build()
            .expect("cache HTTP client")
    })
}
pub(crate) async fn load(
    s: &AppState,
    id: &str,
    rev: &str,
    path: &str,
    sha: &str,
    size: u64,
    full: &Path,
) -> anyhow::Result<Option<(Bytes, &'static str)>> {
    let mut settings = s
        .cache_service_settings
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Ok(url) = std::env::var("TILES3D_CACHE_URL") {
        settings.enabled = !url.is_empty();
        settings.base_url = url;
        settings.token = std::env::var("TILES3D_CACHE_TOKEN").unwrap_or_default();
    }
    if !settings.enabled
        || settings.base_url.is_empty()
        || settings.token.is_empty()
        || size > MAX as u64
    {
        return Ok(None);
    }
    // Bound peak in-flight model buffers; overloaded requests retain the streaming file path.
    static LIMIT: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    let Ok(_permit) = LIMIT
        .get_or_init(|| tokio::sync::Semaphore::new(4))
        .try_acquire()
    else {
        return Ok(None);
    };
    let mut url = reqwest::Url::parse(&format!(
        "{}/api/v1/objects3d/",
        settings.base_url.trim_end_matches('/')
    ))?;
    {
        let mut parts = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid cache URL"))?;
        parts.pop_if_empty().push(id).push(rev);
        for p in path.split('/') {
            parts.push(p);
        }
    }
    let mut status = "ERROR";
    if let Ok(mut response) = client()
        .get(url.clone())
        .bearer_auth(&settings.token)
        .send()
        .await
    {
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            status = "MISS";
        } else if response.status().is_success() {
            let mut data = Vec::new();
            let mut valid = true;
            loop {
                match response.chunk().await {
                    Ok(Some(c)) if data.len() + c.len() <= MAX => data.extend_from_slice(&c),
                    Ok(None) => break,
                    _ => {
                        valid = false;
                        break;
                    }
                }
            }
            if valid && data.len() as u64 == size && format!("{:x}", Sha256::digest(&data)) == sha {
                return Ok(Some((Bytes::from(data), "HIT")));
            }
        }
    }
    // On a failed lookup serve the source without another network timeout.
    if status == "ERROR" {
        return Ok(None);
    }
    let data = tokio::fs::read(full).await?;
    anyhow::ensure!(
        data.len() as u64 == size && format!("{:x}", Sha256::digest(&data)) == sha,
        "published object integrity mismatch"
    );
    let data = Bytes::from(data);
    if !client()
        .put(url)
        .bearer_auth(&settings.token)
        .header("x-content-sha256", sha)
        .body(data.clone())
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
    {
        status = "ERROR";
    }
    Ok(Some((data, status)))
}
