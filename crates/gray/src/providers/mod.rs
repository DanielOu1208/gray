//! Installed provider declarations, sidecar RPCs, and model-catalog dispatch.

pub mod catalog;
pub mod registry;
pub mod runtime;

pub use catalog::{
    DynamicProvider, connect_dynamic_provider, fetch_models_for_config, profile_for_provider,
    resolve_provider_connection,
};
pub use registry::{InstalledProvider, ProviderCache, ProviderRegistry, refresh_plugin};
pub use runtime::{ProviderRuntime, SidecarProviderRpc};
