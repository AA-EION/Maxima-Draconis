use crate::{
    lsx::{
        connection::LockedConnectionState,
        request::LSXRequestError,
        types::{
            LSXAreChunksInstalled, LSXAreChunksInstalledResponse,
            LSXIsProgressiveInstallationAvailable, LSXIsProgressiveInstallationAvailableResponse,
            LSXResponseType,
        },
    },
    make_lsx_handler_response,
};

pub async fn handle_pi_availability_request(
    _: LockedConnectionState,
    request: LSXIsProgressiveInstallationAvailable,
) -> Result<Option<LSXResponseType>, LSXRequestError> {
    // Echo back the same ItemId the client sent — upstream Maxima hardcoded
    // one game's offer id, which would mismatch every other game.
    make_lsx_handler_response!(Response, IsProgressiveInstallationAvailableResponse, {
        attr_Available: false,
        attr_ItemId: request.attr_ItemId,
    })
}

pub async fn handle_pi_installed_chunks_request(
    _: LockedConnectionState,
    request: LSXAreChunksInstalled,
) -> Result<Option<LSXResponseType>, LSXRequestError> {
    make_lsx_handler_response!(Response, AreChunksInstalledResponse, {
        attr_ItemId: request.attr_ItemId,
        attr_Installed: true,
        chunk_ids: request.chunk_ids,
    })
}
