//! Host-owned credential storage and refresh coordination.

pub mod broker;
pub mod store;

pub use broker::{PluginCredentialSource, shared_plugin_source};
pub use store::{
    AuthLock, CredentialStore, StoredCredential, load_plugin_credential, remove_plugin_owner,
    save_plugin_credential,
};
