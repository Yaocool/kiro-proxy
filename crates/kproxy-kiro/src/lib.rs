//! Kiro/AWS upstream transport and event-stream decoding.

pub mod catalog;
pub mod client;
pub mod endpoint;
pub mod event_stream;

pub use catalog::{
    static_models, static_models_for_subscription, static_subscription_can_serve, StaticModel,
    STATIC_MODEL_CATALOG,
};
pub use client::{
    KiroClient, KiroError, KiroResponse, ModelInfo, UsageInfo, UsageLimits, UsageUserInfo,
};
pub use endpoint::{EndpointCache, EndpointDefinition, EndpointKey, EndpointPurpose};
pub use event_stream::{EventStreamDecoder, KiroCitation, KiroEvent};
pub use kproxy_translate::{WebSearchResult, WebSearchResults};
