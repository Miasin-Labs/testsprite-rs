//! `testsprite_check_account_info`. Mirrors `tools/checkAccountInfo.ts`.

use serde_json::{Value, json};

use crate::backend::BackendClient;
use crate::envs;

/// Mask an API key for display: first 13 + "..." + last 5 (matches the plugin).
pub fn mask_api_key(key: &str) -> String {
    if key.len() <= 10 {
        return format!("{}...", &key[..key.len().min(5)]);
    }
    format!("{}...{}", &key[..13], &key[key.len() - 5..])
}

pub async fn check_account_info() -> Value {
    let Some(key) = envs::api_key() else {
        return json!({
            "status": "No API Key",
            "message": format!(
                "No API key found. Create one at {}/dashboard/settings/apikey and set API_KEY.",
                envs::testsprite_url()
            ),
        });
    };

    let client = BackendClient::new(key.clone());
    match client.get_account_info().await {
        Ok(info) => json!({
            "firstName": info.first_name,
            "lastName": info.last_name,
            "emailAddress": info.user,
            "subPlan": info.sub_plan,
            "credits": info.credits,
            "totalTests": info.total_tests,
        }),
        Err(_) => json!({
            "status": "Invalid TestSprite API Key",
            "maskedApiKey": mask_api_key(&key),
            "message": format!(
                "API key was detected but no account was found. Create a new one at {}/dashboard/settings/apikey.",
                envs::testsprite_url()
            ),
        }),
    }
}
