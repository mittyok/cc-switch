#![allow(non_snake_case)]

use crate::app_config::{EmbeddingConfig, EmbeddingProvider, MultiAppConfig};

/// 获取所有 Embedding Provider
#[tauri::command]
pub async fn get_embedding_providers() -> Result<EmbeddingConfig, String> {
    log::info!("get_embedding_providers called");
    let config = MultiAppConfig::load().map_err(|e| {
        log::error!("Failed to load config: {}", e);
        e.to_string()
    })?;
    log::info!("get_embedding_providers success: {:?}", config.embedding);
    Ok(config.embedding)
}

/// 添加或更新一个 Embedding Provider
#[tauri::command]
pub async fn upsert_embedding_provider(provider: EmbeddingProvider) -> Result<(), String> {
    log::info!("upsert_embedding_provider called: {:?}", provider);
    let mut config = MultiAppConfig::load().map_err(|e| {
        log::error!("Failed to load config: {}", e);
        e.to_string()
    })?;
    config
        .embedding
        .providers
        .insert(provider.id.clone(), provider);
    config.save().map_err(|e| {
        log::error!("Failed to save config: {}", e);
        e.to_string()
    })?;
    log::info!("upsert_embedding_provider success");
    Ok(())
}

/// 删除一个 Embedding Provider
#[tauri::command]
pub async fn delete_embedding_provider(id: String) -> Result<(), String> {
    let mut config = MultiAppConfig::load().map_err(|e| e.to_string())?;
    config.embedding.providers.remove(&id);
    // 如果删除的是当前选中的，清除 current
    if config.embedding.current.as_ref() == Some(&id) {
        config.embedding.current = None;
    }
    config.save().map_err(|e| e.to_string())?;
    Ok(())
}

/// 设置当前默认的 Embedding Provider
#[tauri::command]
pub async fn set_current_embedding_provider(id: Option<String>) -> Result<(), String> {
    let mut config = MultiAppConfig::load().map_err(|e| e.to_string())?;
    config.embedding.current = id;
    config.save().map_err(|e| e.to_string())?;
    Ok(())
}
