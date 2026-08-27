//! The Node-facing Stream client. Owns the server credentials and hands out
//! [`NativeCall`] handles.

use getstream::{ClientConfig, Stream};
use napi_derive::napi;

use crate::call::NativeCall;
use crate::error::sdk_error;

#[napi]
pub struct NativeStreamClient {
    stream: Stream,
}

#[napi]
impl NativeStreamClient {
    /// Build a server-authenticated client. Tokens for the participant path are
    /// minted inside the Rust core, so the secret never reaches JavaScript's
    /// media path.
    #[napi(constructor)]
    pub fn new(
        api_key: String,
        api_secret: String,
        base_url: Option<String>,
    ) -> napi::Result<Self> {
        let config = match base_url {
            Some(base_url) => ClientConfig {
                base_url,
                ..ClientConfig::default()
            },
            None => ClientConfig::default(),
        };
        let stream = Stream::with_config(api_key, api_secret, config).map_err(sdk_error)?;
        Ok(Self { stream })
    }

    /// A handle to one call. Cheap: no request is made until the call is joined.
    #[napi]
    pub fn call(&self, call_type: String, id: String) -> NativeCall {
        NativeCall::new(self.stream.video().call(call_type, id))
    }
}
