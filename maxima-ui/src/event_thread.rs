use maxima::rtm::client::BasicPresence;

pub struct EventThreadFriendStatusResponse {
    pub id: String,
    pub basic: BasicPresence,
    pub status: String,
    pub game: Option<String>,
}

pub enum MaximaEventResponse {
    FriendStatusResponse(EventThreadFriendStatusResponse),
}
