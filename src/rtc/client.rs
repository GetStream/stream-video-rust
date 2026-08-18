//! A lower-level participant client (`RtcClient`) constructed from an API key
//! and a **user token**, mirroring videosdk / the JS `StreamVideoClient` style.
//!
//! Most callers should use [`crate::Stream`] + [`crate::Call::join`], which mints
//! the user token server-side. `RtcClient` is handy for tests and for callers
//! that already hold a user token and only need the participant path.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::client::{Client, ClientConfig, NetworkLimits};
use crate::error::Result as CrateResult;
use crate::token::{self, TokenOptions};

use super::error::{Result, RtcError};
use super::join::{CallEvent, CallStateSnapshot, CallingState, JoinCallData, RtcCore};
use super::local_track::{LocalAudioTrack, LocalTrack, LocalVideoTrack};
use super::proto::models::TrackType;
use super::remote_track::{RemoteParticipant, RemoteTrack};
use super::subscriptions::{SubscriptionConfig, SubscriptionTarget};

/// Boxed future returned by an RTC [`TokenProvider`].
pub type TokenFuture = Pin<Box<dyn Future<Output = CrateResult<String>> + Send + 'static>>;

/// Asynchronous user-token provider used to recover from Stream error code 40.
///
/// The provider is called once before the initial join and at most once after a
/// token-expired response. Concurrent provider calls are serialized by the
/// owning call.
pub trait TokenProvider: Send + Sync {
    /// Load a user JWT.
    fn load_token(&self) -> TokenFuture;
}

impl<F, Fut> TokenProvider for F
where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = CrateResult<String>> + Send + 'static,
{
    fn load_token(&self) -> TokenFuture {
        Box::pin((self)())
    }
}

#[derive(Clone)]
pub(crate) enum UserTokenSource {
    Static(String),
    Provider(Arc<dyn TokenProvider>),
    ServerMinted {
        client: Arc<Client>,
        user_id: String,
        call_cid: String,
        expiration: Duration,
    },
}

impl std::fmt::Debug for UserTokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Static(_) => f.debug_tuple("Static").field(&"<redacted>").finish(),
            Self::Provider(_) => f.debug_tuple("Provider").field(&"<provider>").finish(),
            Self::ServerMinted {
                user_id,
                call_cid,
                expiration,
                ..
            } => f
                .debug_struct("ServerMinted")
                .field("user_id", user_id)
                .field("call_cid", call_cid)
                .field("expiration", expiration)
                .finish_non_exhaustive(),
        }
    }
}

impl UserTokenSource {
    pub(crate) async fn load(&self, expected_user_id: &str) -> Result<String> {
        let token = match self {
            Self::Static(token) => token.clone(),
            Self::Provider(provider) => provider.load_token().await.map_err(RtcError::from)?,
            Self::ServerMinted {
                client,
                user_id,
                call_cid,
                expiration,
            } => token::create_user_token(
                client.api_secret(),
                user_id,
                &TokenOptions {
                    expiration: Some(*expiration),
                    call_cids: Some(vec![call_cid.clone()]),
                    ..Default::default()
                },
            )
            .map_err(RtcError::from)?,
        };
        token::validate_operational_token(&token, expected_user_id).map_err(RtcError::from)?;
        Ok(token)
    }

    pub(crate) async fn load_with_expiry_retry(&self, expected_user_id: &str) -> Result<String> {
        match self.load(expected_user_id).await {
            Err(error) if error.is_token_expired() && self.can_refresh() => {
                self.load(expected_user_id).await
            }
            result => result,
        }
    }

    pub(crate) fn can_refresh(&self) -> bool {
        !matches!(self, Self::Static(_))
    }

    pub(crate) fn refreshes_before_full_reconnect(&self) -> bool {
        matches!(self, Self::ServerMinted { .. })
    }
}

/// A participant-only client bound to a single user token.
#[derive(Clone)]
pub struct RtcClient {
    client: Arc<Client>,
    token_source: UserTokenSource,
    disconnection_timeout: Duration,
}

impl std::fmt::Debug for RtcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtcClient")
            .field("client", &self.client)
            .field("token_source", &self.token_source)
            .field("disconnection_timeout", &self.disconnection_timeout)
            .finish()
    }
}

impl RtcClient {
    /// Build from an API key and a user JWT, using the default coordinator URL
    /// and connection settings.
    pub fn new(api_key: impl Into<String>, user_token: impl Into<String>) -> CrateResult<Self> {
        Self::with_config(api_key, user_token, ClientConfig::default())
    }

    /// Build with a custom [`ClientConfig`] (e.g. an alternate base URL).
    pub fn with_config(
        api_key: impl Into<String>,
        user_token: impl Into<String>,
        config: ClientConfig,
    ) -> CrateResult<Self> {
        // The participant path never mints the server token, but the shared
        // HTTP client needs a non-empty secret to construct; pass a placeholder
        // that is never used to sign participant requests (they use the user
        // token). This keeps a single connection-pool implementation.
        let client = Client::new(api_key.into(), "webrtc-user".to_owned(), config)?;
        Ok(Self {
            client: Arc::new(client),
            token_source: UserTokenSource::Static(user_token.into()),
            disconnection_timeout: Duration::ZERO,
        })
    }

    /// Build with custom connection settings and payload limits.
    pub fn with_config_and_limits(
        api_key: impl Into<String>,
        user_token: impl Into<String>,
        config: ClientConfig,
        limits: NetworkLimits,
    ) -> CrateResult<Self> {
        let client =
            Client::new_with_limits(api_key.into(), "webrtc-user".to_owned(), config, limits)?;
        Ok(Self {
            client: Arc::new(client),
            token_source: UserTokenSource::Static(user_token.into()),
            disconnection_timeout: Duration::ZERO,
        })
    }

    /// Build with an asynchronous token provider.
    ///
    /// Provider tokens are validated for user identity and time claims before
    /// operational use. If Stream rejects a token as expired, the provider is
    /// called once more and the failed join is retried once.
    pub fn with_token_provider<P>(
        api_key: impl Into<String>,
        token_provider: P,
    ) -> CrateResult<Self>
    where
        P: TokenProvider + 'static,
    {
        Self::with_token_provider_and_limits(api_key, token_provider, NetworkLimits::default())
    }

    /// Build with an asynchronous token provider and custom payload limits.
    pub fn with_token_provider_and_limits<P>(
        api_key: impl Into<String>,
        token_provider: P,
        limits: NetworkLimits,
    ) -> CrateResult<Self>
    where
        P: TokenProvider + 'static,
    {
        let client = Client::new_with_limits(
            api_key.into(),
            "webrtc-user".to_owned(),
            ClientConfig::default(),
            limits,
        )?;
        Ok(Self {
            client: Arc::new(client),
            token_source: UserTokenSource::Provider(Arc::new(token_provider)),
            disconnection_timeout: Duration::ZERO,
        })
    }

    /// Set the maximum reconnect duration. Zero keeps reconnecting indefinitely.
    #[must_use]
    pub fn with_disconnection_timeout(mut self, timeout: Duration) -> Self {
        self.disconnection_timeout = timeout;
        self
    }

    /// Join `<call_type>:<call_id>` and return a live [`RtcCall`] handle.
    pub async fn join(
        &self,
        call_type: impl Into<String>,
        call_id: impl Into<String>,
        data: JoinCallData,
    ) -> Result<RtcCall> {
        let core = RtcCore::new(self.client.clone(), call_type.into(), call_id.into());
        core.set_disconnection_timeout(self.disconnection_timeout);
        core.join_with_token_source(self.token_source.clone(), data)
            .await?;
        Ok(RtcCall { core })
    }
}

/// A joined call handle from [`RtcClient::join`].
#[derive(Clone)]
pub struct RtcCall {
    core: Arc<RtcCore>,
}

impl RtcCall {
    /// Subscribe to the typed SFU event stream.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<CallEvent> {
        self.core.subscribe()
    }

    /// Register a callback for typed call events.
    pub fn on<F>(&self, callback: F) -> tokio::task::AbortHandle
    where
        F: Fn(CallEvent) + Send + 'static,
    {
        self.core.on(callback)
    }

    /// Remove a callback registered with [`Self::on`].
    pub fn off(&self, handler: &tokio::task::AbortHandle) {
        self.core.off(handler);
    }

    /// The current calling state.
    pub fn calling_state(&self) -> CallingState {
        self.core.state()
    }

    /// The live session id.
    pub async fn session_id(&self) -> Option<String> {
        self.core.session_id().await
    }

    /// A snapshot of the participants currently known to the SFU.
    pub fn participants(&self) -> Vec<RemoteParticipant> {
        self.core.participants()
    }

    /// Return the latest participant, count, pin, grant, and session snapshot.
    pub fn call_state(&self) -> CallStateSnapshot {
        self.core.call_state()
    }

    /// Register the callback fired for every inbound media track.
    pub fn on_track<F>(&self, callback: F)
    where
        F: Fn(RemoteTrack) + Send + Sync + 'static,
    {
        self.core.on_track(callback);
    }

    /// Publish a local audio track.
    pub async fn publish_audio(&self, track: LocalAudioTrack) -> Result<()> {
        self.core.publish(LocalTrack::Audio(track)).await
    }

    /// Publish a local video track.
    pub async fn publish_video(&self, track: LocalVideoTrack) -> Result<()> {
        self.core
            .publish(LocalTrack::Video {
                track,
                track_type: TrackType::Video,
            })
            .await
    }

    /// Publish a local screen-share track.
    pub async fn publish_screen_share(&self, track: LocalVideoTrack) -> Result<()> {
        self.core
            .publish(LocalTrack::Video {
                track,
                track_type: TrackType::ScreenShare,
            })
            .await
    }

    /// Publish an audio track associated with a screen share.
    pub async fn publish_screen_share_audio(&self, track: LocalAudioTrack) -> Result<()> {
        self.core.publish(LocalTrack::ScreenShareAudio(track)).await
    }

    /// Temporarily mute a published track kind while preserving its sender.
    pub async fn mute_track(&self, track_type: TrackType) -> Result<()> {
        self.core.set_track_muted(track_type, true).await
    }

    /// Resume a temporarily muted published track kind.
    pub async fn unmute_track(&self, track_type: TrackType) -> Result<()> {
        self.core.set_track_muted(track_type, false).await
    }

    /// Enable SFU-side noise cancellation.
    pub async fn start_noise_cancellation(&self) -> Result<()> {
        self.core.start_noise_cancellation().await
    }

    /// Disable SFU-side noise cancellation.
    pub async fn stop_noise_cancellation(&self) -> Result<()> {
        self.core.stop_noise_cancellation().await
    }

    /// Stop publishing a local media track.
    pub async fn stop_publish(&self, track: LocalTrack) -> Result<()> {
        self.core.stop_publish(track).await
    }

    /// Update the remote media subscription policy.
    pub async fn update_subscriptions(&self, config: SubscriptionConfig) -> Result<()> {
        self.core.update_subscriptions(config).await
    }

    /// Subscribe to an exact set of participant-session tracks.
    pub async fn update_subscription_targets(
        &self,
        targets: Vec<SubscriptionTarget>,
    ) -> Result<()> {
        self.core.update_subscription_targets(targets).await
    }

    /// Enable or disable incoming video from every remote participant.
    pub async fn set_incoming_video_enabled(&self, enabled: bool) -> Result<()> {
        self.core.set_incoming_video_enabled(enabled).await
    }

    /// Leave the call, closing the SFU connection and PeerConnections.
    pub async fn leave(&self) -> Result<()> {
        self.core.leave("user requested leave").await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;

    use super::*;
    use crate::error::TokenError;

    fn inspected_token(claims: serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("serialize claims"));
        let signature = URL_SAFE_NO_PAD.encode([0_u8; 32]);
        format!("{header}.{payload}.{signature}")
    }

    #[test]
    fn client_debug_redacts_static_token() {
        let client = RtcClient::new("key", "rtc-token-must-not-leak").expect("participant client");
        let debug = format!("{client:?}");
        assert!(!debug.contains("rtc-token-must-not-leak"));
        assert!(debug.contains("<redacted>"));
    }

    #[tokio::test]
    async fn static_expired_token_surfaces_typed_error() {
        let source = UserTokenSource::Static(inspected_token(json!({
            "user_id": "user",
            "iat": 1,
            "exp": 2
        })));
        let error = source.load("user").await.expect_err("expired token");
        assert!(matches!(
            error,
            RtcError::TokenValidation(TokenError::Expired { exp: 2, .. })
        ));
    }

    #[tokio::test]
    async fn provider_loads_fresh_tokens() {
        let loads = Arc::new(AtomicUsize::new(0));
        let provider_loads = loads.clone();
        let token = inspected_token(json!({"user_id": "user"}));
        let source = UserTokenSource::Provider(Arc::new(move || {
            provider_loads.fetch_add(1, Ordering::SeqCst);
            let token = token.clone();
            async move { Ok(token) }
        }));

        source.load("user").await.expect("first token");
        source.load("user").await.expect("refreshed token");
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn provider_reloads_once_when_first_token_is_expired() {
        let loads = Arc::new(AtomicUsize::new(0));
        let provider_loads = loads.clone();
        let expired = inspected_token(json!({"user_id": "user", "exp": 1}));
        let fresh = inspected_token(json!({"user_id": "user"}));
        let source = UserTokenSource::Provider(Arc::new(move || {
            let load = provider_loads.fetch_add(1, Ordering::SeqCst);
            let token = if load == 0 {
                expired.clone()
            } else {
                fresh.clone()
            };
            async move { Ok(token) }
        }));

        source
            .load_with_expiry_retry("user")
            .await
            .expect("refreshed token");
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn server_minted_tokens_are_finite_and_call_scoped() {
        let client = Arc::new(
            Client::new(
                "key".to_owned(),
                "secret".to_owned(),
                ClientConfig::default(),
            )
            .expect("client"),
        );
        let source = UserTokenSource::ServerMinted {
            client: client.clone(),
            user_id: "user".to_owned(),
            call_cid: "default:call".to_owned(),
            expiration: Duration::from_secs(600),
        };

        let token = source.load("user").await.expect("mint token");
        let claims = token::decode_token(client.api_secret(), &token).expect("verify token");
        assert_eq!(claims.call_cids, Some(vec!["default:call".to_owned()]));
        assert_eq!(
            claims.exp.zip(claims.iat).map(|(exp, iat)| exp - iat),
            Some(600)
        );
    }
}
