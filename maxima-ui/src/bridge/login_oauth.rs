use crate::bridge_thread::{BackendError, InteractThreadLoginResponse, MaximaLibResponse};
use egui::Context;
use maxima::{
    core::{
        auth::{context::AuthContext, login, nucleus_token_exchange, storage::AuthStorage},
        service_layer::ServiceLayerError,
        LockedMaxima,
    },
    util::native::take_foreground_focus,
};
use std::{sync::mpsc::Sender, time::Duration};

pub async fn login_oauth(
    maxima_arc: LockedMaxima,
    channel: Sender<MaximaLibResponse>,
    ctx: &Context,
) -> Result<(), BackendError> {
    let maxima = maxima_arc.lock().await;

    {
        let mut auth_storage = maxima.auth_storage().lock().await;
        let mut context = AuthContext::new()?;
        login::begin_oauth_login_flow(&mut context).await?;
        let token_res = nucleus_token_exchange(&context).await?;
        auth_storage.add_account(&token_res).await?;
    }

    let user = maxima.local_user().await?;
    let message = MaximaLibResponse::LoginResponse(Ok(InteractThreadLoginResponse {
        you: user.player().as_ref().ok_or(ServiceLayerError::MissingField)?.to_owned(),
    }));

    channel.send(message)?;

    take_foreground_focus()?;
    ctx.request_repaint();
    Ok(())
}

/// Resolves once another process of this installation has saved a valid
/// login. The Maxima server this UI starts runs its own EA login (it opens
/// the browser itself), and that login lands in the server, not here; without
/// this the UI would sit on its login screen after the user finished logging
/// in. Only re-validates when the saved login file changes.
pub async fn saved_login_appeared() {
    let mut seen = None;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let saved_at = AuthStorage::saved_at();
        if saved_at.is_none() || saved_at == seen {
            continue;
        }
        seen = saved_at;

        let Ok(saved) = AuthStorage::load() else { continue };
        if saved.lock().await.logged_in().await.unwrap_or(false) {
            return;
        }
    }
}
