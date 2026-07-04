use std::time::Duration;

use crate::core::{
    auth::storage::LockedAuthStorage,
    cache::DynamicCache,
    service_layer::{
        ServiceAvailableBuild, ServiceAvailableBuilds, ServiceAvailableBuildsBuilder,
        ServiceAvailableBuildsRequestBuilder, ServiceDownloadUrlMetadata,
        ServiceDownloadUrlRequestBuilder, ServiceLayerClient, ServiceLayerError,
        SERVICE_REQUEST_AVAILABLEBUILDS, SERVICE_REQUEST_DOWNLOADURL,
    },
};

pub mod downloader;
pub mod manager;
pub mod zip;
pub mod zlib;

pub struct ContentService {
    service_layer: ServiceLayerClient,
    request_cache: DynamicCache<String>,
}

impl ContentService {
    pub fn new(auth: LockedAuthStorage) -> Self {
        let request_cache = DynamicCache::new(
            100,
            Duration::from_secs(30 * 60),
            Duration::from_secs(5 * 60),
        );

        Self {
            service_layer: ServiceLayerClient::new(auth),
            request_cache,
        }
    }

    pub async fn available_builds(
        &self,
        offer_id: &str,
    ) -> Result<ServiceAvailableBuilds, ServiceLayerError> {
        let cache_key = "builds_".to_owned() + offer_id;
        if let Some(cached) = self.request_cache.get(&cache_key) {
            return Ok(cached);
        }

        let builds: Vec<ServiceAvailableBuild> = self
            .service_layer
            .request(
                SERVICE_REQUEST_AVAILABLEBUILDS,
                ServiceAvailableBuildsRequestBuilder::default()
                    .offer_id(offer_id.to_owned())
                    .build()?,
            )
            .await?;

        let builds = ServiceAvailableBuildsBuilder::default()
            .builds(builds)
            .build()?;
        self.request_cache.insert(cache_key, builds.clone());
        Ok(builds)
    }

    pub async fn download_url(
        &self,
        offer_id: &str,
        build_id: Option<&str>,
    ) -> Result<ServiceDownloadUrlMetadata, ServiceLayerError> {
        let cache_key = "download_url_".to_owned() + offer_id + "_" + build_id.unwrap_or("live");
        if let Some(cached) = self.request_cache.get(&cache_key) {
            return Ok(cached);
        }

        // Read-only query — retry a few times with backoff. EA's API can
        // stall/timeout transiently (the client has a 60s cap), and one
        // flaky response shouldn't fail an install queue.
        let mut last_err = None;
        for attempt in 0u32..4 {
            let request = ServiceDownloadUrlRequestBuilder::default()
                .offer_id(offer_id.to_owned())
                .build_id(build_id.unwrap_or_default().to_owned())
                .build()?;

            match self
                .service_layer
                .request::<_, ServiceDownloadUrlMetadata>(SERVICE_REQUEST_DOWNLOADURL, request)
                .await
            {
                Ok(url) => {
                    self.request_cache.insert(cache_key, url.clone());
                    return Ok(url);
                }
                Err(err) => {
                    log::warn!("download_url attempt {}/4 failed: {}", attempt + 1, err);
                    last_err = Some(err);
                    if attempt < 3 {
                        tokio::time::sleep(std::time::Duration::from_secs(2u64 << attempt))
                            .await;
                    }
                }
            }
        }
        Err(last_err.expect("at least one attempt"))
    }
}
