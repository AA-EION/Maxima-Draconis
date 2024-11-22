use std::io;

#[derive(thiserror::Error, Debug)]
pub enum CoreError {
    #[error("IO error: {0}")]
    IoError(#[from] io::Error),
    #[error("Anyhow error: {0}")]
    AnyhowError(#[from] anyhow::Error),
    #[error("Unauthenticated")]
    Unauthenticated,
    #[error("Offer not found")]
    OfferNotFound,
    #[error("Cannot launch game '{0}' as it is not installed")]
    LaunchGameNotInstalled(String),
    #[error("Game path must be specified when launching in OnlineOffline mode")]
    LaunchGamePathRequired,
    #[error("The path to the game was not able to be automatically found")]
    LaunchGamePathNotFound,
    #[error("Content ID was specified as an offer ID when launching in OnlineOffline mode: {0}")]
    LaunchContentIdRequired(String),
    #[error("Offline mode is not yet supported")]
    LaunchOfflineUnsupported
}
