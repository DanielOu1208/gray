//! Host-owned plugin provider authentication for `/connect`.

use std::path::Path;

use anyhow::{Context, anyhow};
use gray_core::credential::CredentialEnvelope;
use gray_plugin::ProviderModelsRequest;
use tokio_util::sync::CancellationToken;

use crate::auth::CredentialStore;
use crate::config::Config;
use crate::providers::registry::{InstalledProvider, ProviderRpc};
use crate::providers::runtime::ProviderRuntime;
use crate::setup::catalog::{
    SavedConfig, load_saved_config_at, lock_saved_config_at, save_saved_config_at,
    saved_config_path,
};

/// UI-safe login progress. No event ever carries a token, transcript, or
/// provider response body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginLoginProgress {
    Started { verification_uri: String },
    Pending,
    CredentialSaved,
    Models(Vec<(String, String)>),
    ModelsUnavailable(String),
    Cancelled,
    Failed(String),
}

fn model_tuple(model: &gray_plugin::ProviderModel) -> (String, String) {
    let name = if model.name.trim().is_empty() {
        model.id.clone()
    } else {
        model.name.clone()
    };
    (model.id.clone(), name)
}

/// Run one host-owned browser login against a provider sidecar.
///
/// The sidecar receives only host-owned identity and the credential payload
/// it returned; Gray saves the credential before model discovery and shuts
/// the sidecar down on every exit path.
pub async fn run_plugin_login(
    installed: InstalledProvider,
    cancel: CancellationToken,
    progress: tokio::sync::mpsc::UnboundedSender<PluginLoginProgress>,
) -> anyhow::Result<()> {
    let home = crate::setup::gray_home()?;
    let runtime = ProviderRuntime::start(installed.clone()).await?;
    let rpc = runtime.rpc();
    let operation = rpc
        .auth_start(&installed.provider.id, &installed.auth_method.id)
        .await
        .map_err(|_| anyhow!("provider login unavailable"))?;
    let _ = progress.send(PluginLoginProgress::Started {
        verification_uri: operation.verification_uri.clone(),
    });
    crate::feedback::open_in_browser(&operation.verification_uri);

    let mut delay = std::time::Duration::from_millis(1_000);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = rpc.auth_cancel(&operation.operation_id).await;
                let _ = progress.send(PluginLoginProgress::Cancelled);
                return Ok(());
            }
            _ = tokio::time::sleep(delay) => {}
        }
        if cancel.is_cancelled() {
            let _ = rpc.auth_cancel(&operation.operation_id).await;
            let _ = progress.send(PluginLoginProgress::Cancelled);
            return Ok(());
        }
        match rpc.auth_poll(&operation.operation_id).await {
            Ok(gray_plugin::ProviderAuthPoll::Pending { retry_after_ms }) => {
                delay = std::time::Duration::from_millis(
                    retry_after_ms.unwrap_or(2_000).clamp(200, 10_000),
                );
                let _ = progress.send(PluginLoginProgress::Pending);
            }
            Ok(gray_plugin::ProviderAuthPoll::Completed(material)) => {
                if cancel.is_cancelled() {
                    let _ = rpc.auth_cancel(&operation.operation_id).await;
                    let _ = progress.send(PluginLoginProgress::Cancelled);
                    return Ok(());
                }
                let envelope = CredentialEnvelope::new(
                    installed.plugin.clone(),
                    installed.provider.id.clone(),
                    installed.auth_method.id.clone(),
                    installed.profile_binding.clone(),
                    material,
                )
                .map_err(|e| anyhow!("provider credential rejected: {e}"))?;
                envelope
                    .credential
                    .validate()
                    .map_err(|e| anyhow!("provider credential rejected: {e}"))?;
                if envelope.credential.secrets.is_empty() {
                    let _ = progress.send(PluginLoginProgress::Failed(
                        "provider login returned no credential".into(),
                    ));
                    return Ok(());
                }
                let store = CredentialStore::new(home.join("auth.json"));
                store
                    .put_plugin(envelope.clone())
                    .context("cannot save provider credential")?;
                let _ = progress.send(PluginLoginProgress::CredentialSaved);
                let request = ProviderModelsRequest {
                    provider: installed.provider.id.clone(),
                    auth_method: installed.auth_method.id.clone(),
                    profile_binding: installed.profile_binding.clone(),
                    credential: envelope,
                };
                match rpc.models(request).await {
                    Ok(catalog) => {
                        let models = catalog.models.iter().map(model_tuple).collect::<Vec<_>>();
                        let _ = progress.send(PluginLoginProgress::Models(models));
                    }
                    Err(_) => {
                        let _ = progress.send(PluginLoginProgress::ModelsUnavailable(
                            "provider model discovery unavailable".into(),
                        ));
                    }
                }
                return Ok(());
            }
            Ok(gray_plugin::ProviderAuthPoll::Failed(_)) => {
                let _ = progress.send(PluginLoginProgress::Failed(
                    "provider rejected login".into(),
                ));
                return Ok(());
            }
            Ok(gray_plugin::ProviderAuthPoll::Cancelled) => {
                let _ = progress.send(PluginLoginProgress::Cancelled);
                return Ok(());
            }
            Ok(gray_plugin::ProviderAuthPoll::OperationLost) => {
                let _ = progress.send(PluginLoginProgress::Failed(
                    "provider login was lost".into(),
                ));
                return Ok(());
            }
            Err(_) => {
                let _ = progress.send(PluginLoginProgress::Failed(
                    "provider login unavailable".into(),
                ));
                return Ok(());
            }
        }
    }
}

/// Persist a plugin-backed provider selection: never a plaintext key.
pub fn activate_plugin_connection(
    config: &mut Config,
    installed: &InstalledProvider,
    model: &str,
) -> anyhow::Result<()> {
    let path = saved_config_path()?;
    activate_plugin_connection_at(config, installed, model, &path)
}

fn activate_plugin_connection_at(
    config: &mut Config,
    installed: &InstalledProvider,
    model: &str,
    path: &Path,
) -> anyhow::Result<()> {
    let _lock = lock_saved_config_at(path).ok();
    let mut saved = load_saved_config_at(path);
    config.base_url = installed.provider.transport.base_url.to_string();
    config.api_key = None;
    config.provider_id = installed.provider_id();
    config.credential_source = "plugin".to_string();
    config.auth_ref = installed.auth_ref();
    config.model = (!model.trim().is_empty()).then(|| model.to_string());
    saved.base_url = Some(config.base_url.clone());
    saved.api_key = None;
    saved.auth_mode = Some(installed.auth_method.kind.clone());
    saved.provider_id = config.provider_id.clone();
    saved.credential_source = config.credential_source.clone();
    saved.auth_ref = config.auth_ref.clone();
    saved.model = config.model.clone();
    save_saved_config_at(path, &saved)
}

fn clear_plugin_selection(config: &mut Config, saved: &mut SavedConfig, provider_id: &str) {
    if saved.provider_id == provider_id {
        saved.provider_id.clear();
        saved.credential_source.clear();
        saved.auth_ref.clear();
        saved.api_key = None;
        saved.auth_mode = None;
        saved.model = None;
    }
    if config.provider_id == provider_id {
        config.provider_id.clear();
        config.credential_source.clear();
        config.auth_ref.clear();
        config.api_key = None;
        config.model = None;
    }
}

fn api_key_selection_at(config: &mut Config, path: &Path) {
    config.provider_id.clear();
    config.credential_source.clear();
    config.auth_ref.clear();
    let _lock = lock_saved_config_at(path).ok();
    let mut saved = load_saved_config_at(path);
    saved.provider_id.clear();
    saved.credential_source.clear();
    saved.auth_ref.clear();
    let _ = save_saved_config_at(path, &saved);
}

/// Clear plugin references when an API-key row becomes active.
pub fn select_api_key_connection(config: &mut Config) -> anyhow::Result<()> {
    let path = saved_config_path()?;
    api_key_selection_at(config, &path);
    Ok(())
}

/// Local-first plugin credential removal plus best-effort remote revoke.
pub async fn forget_plugin_connection(
    item: &crate::setup::ConnectItem,
    config: &mut Config,
) -> anyhow::Result<String> {
    let home = crate::setup::gray_home()?;
    let auth_path = home.join("auth.json");
    let saved_path = saved_config_path()?;
    let ConnectAuthPlugin {
        auth_ref,
        profile_binding,
    } = plugin_auth(item)?;
    let installed = crate::providers::resolve_provider_connection(
        &Config {
            provider_id: item.id.clone(),
            credential_source: "plugin".to_string(),
            auth_ref: auth_ref.clone(),
            ..config.clone()
        },
        &home,
    );
    let store = CredentialStore::new(auth_path);
    let credential = store
        .read_plugin(&auth_ref)
        .context("cannot read provider credential")?;
    let removed = store
        .remove(&auth_ref)
        .context("cannot remove provider credential")?;
    if !removed {
        return Err(anyhow!("not installed: {}", item.id));
    }

    let _lock = lock_saved_config_at(&saved_path).ok();
    let mut saved = load_saved_config_at(&saved_path);
    clear_plugin_selection(config, &mut saved, &item.id);
    save_saved_config_at(&saved_path, &saved)?;

    if let (Some(installed), Some(credential)) = (installed, credential)
        && let Ok(runtime) = ProviderRuntime::start(installed.clone()).await
    {
        let request = gray_plugin::ProviderRevokeRequest {
            provider: installed.provider.id.clone(),
            auth_method: installed.auth_method.id.clone(),
            profile_binding: profile_binding.clone(),
            credential,
        };
        let _ = runtime.rpc().revoke(request).await;
    }
    Ok(item.name.clone())
}

struct ConnectAuthPlugin {
    auth_ref: String,
    profile_binding: String,
}

fn plugin_auth(item: &crate::setup::ConnectItem) -> anyhow::Result<ConnectAuthPlugin> {
    match &item.auth {
        crate::setup::ConnectAuth::Plugin {
            auth_ref,
            profile_binding,
        } => Ok(ConnectAuthPlugin {
            auth_ref: auth_ref.clone(),
            profile_binding: profile_binding.clone(),
        }),
        _ => Err(anyhow!("not a plugin provider row")),
    }
}

/// Test seam: same persistence path against an explicit saved config.
#[cfg(test)]
fn activate_for_test(
    config: &mut Config,
    installed: &InstalledProvider,
    model: &str,
    path: &Path,
) -> anyhow::Result<()> {
    activate_plugin_connection_at(config, installed, model, path)
}

#[path = "provider_auth_tests.rs"]
#[cfg(test)]
mod tests;
