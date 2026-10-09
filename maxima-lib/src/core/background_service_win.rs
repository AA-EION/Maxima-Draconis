use log::debug;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::core::error::BackgroundServiceClientError;
use crate::util::dll_injector::DllInjector;
use crate::util::native::NativeError;
use crate::util::registry::{set_up_registry, RegistryError};
use is_elevated::is_elevated;

pub const BACKGROUND_SERVICE_PORT: u16 = 13021;

/// Header every Maxima client sends to the background service. The service
/// rejects requests without it (and any request carrying an `Origin` header):
/// a web page can make the browser hit `127.0.0.1`, but it cannot attach a
/// custom header without a CORS preflight, which the service never answers.
pub const BACKGROUND_SERVICE_CLIENT_HEADER: (&str, &str) = ("x-maxima-client", "1");

#[derive(Default, Serialize, Deserialize)]
pub struct ServiceLibraryInjectionRequest {
    pub pid: u32,
    pub path: String,
}

pub async fn request_library_injection(
    pid: u32,
    path: &str,
) -> Result<(), BackgroundServiceClientError> {
    debug!("Injecting {}", path);

    if is_elevated() {
        let injector = DllInjector::new(pid);
        injector.inject(path)?;
        return Ok(());
    }

    let request = &ServiceLibraryInjectionRequest {
        pid,
        path: path.to_owned(),
    };

    let client = reqwest::Client::new();
    let res = client
        .post(format!(
            "http://127.0.0.1:{}/inject_library",
            BACKGROUND_SERVICE_PORT
        ))
        .header(BACKGROUND_SERVICE_CLIENT_HEADER.0, BACKGROUND_SERVICE_CLIENT_HEADER.1)
        .body(serde_json::to_string(request)?)
        .send()
        .await?;
    if res.status() != StatusCode::OK {
        return Err(BackgroundServiceClientError::Request(res.text().await?));
    }

    Ok(())
}

pub async fn request_registry_setup() -> Result<(), BackgroundServiceClientError> {
    if is_elevated() {
        set_up_registry()?;
        return Ok(());
    }

    reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{}/set_up_registry",
            BACKGROUND_SERVICE_PORT
        ))
        .header(BACKGROUND_SERVICE_CLIENT_HEADER.0, BACKGROUND_SERVICE_CLIENT_HEADER.1)
        .send()
        .await?;
    Ok(())
}
