use std::sync::Arc;
use tokio::sync::RwLock;
use webrtc::peer_connection::configuration::RTCConfiguration;

use crate::game::Game;

#[derive(Clone)]
pub struct AppState {
    pub game: Arc<RwLock<Game>>,
    pub api: Arc<webrtc::api::API>,
    pub config: RTCConfiguration,
    pub allow_versions: Vec<String>,
}
