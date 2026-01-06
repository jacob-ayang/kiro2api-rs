//! Kiro API Provider
//!
//! 核心组件，负责与 Kiro API 通信
//! 支持流式和非流式请求

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONNECTION, CONTENT_TYPE, HOST};
use reqwest::{Client, StatusCode};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::http_client::{build_client, ProxyConfig};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::TokenManager;

/// Kiro API Provider
///
/// 核心组件，负责与 Kiro API 通信
/// 内部使用 Arc<Mutex<_>> 管理 TokenManager 状态，支持线程安全的并发访问
pub struct KiroProvider {
    token_manager: Arc<Mutex<TokenManager>>,
    client: Client,
}

const KIRO_MAX_ATTEMPTS: usize = 3;
const KIRO_RETRY_BASE_DELAY_MS: u64 = 200;
const KIRO_RETRY_MAX_DELAY_MS: u64 = 2_000;

fn retry_backoff(attempt: usize) -> Duration {
    let exp = 1u64 << (attempt.saturating_sub(1).min(10) as u32);
    let base = KIRO_RETRY_BASE_DELAY_MS
        .saturating_mul(exp)
        .min(KIRO_RETRY_MAX_DELAY_MS);
    let jitter = fastrand::u64(0..=(base / 4).max(1));
    Duration::from_millis(base + jitter)
}

fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn is_auth_status(status: StatusCode) -> bool {
    status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN
}

fn is_retryable_reqwest_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect()
}

impl KiroProvider {
    /// 创建新的 KiroProvider 实例
    #[allow(dead_code)]
    pub fn new(token_manager: TokenManager) -> Self {
        Self::with_proxy(token_manager, None)
    }

    /// 创建带代理配置的 KiroProvider 实例
    pub fn with_proxy(token_manager: TokenManager, proxy: Option<ProxyConfig>) -> Self {
        let client = build_client(proxy.as_ref(), 720) // 12 分钟超时
            .expect("创建 HTTP 客户端失败");

        Self {
            token_manager: Arc::new(Mutex::new(token_manager)),
            client,
        }
    }

    /// 使用共享的 TokenManager 创建 Provider（适用于账号池模式）
    pub fn with_shared_token_manager(
        token_manager: Arc<Mutex<TokenManager>>,
        proxy: Option<ProxyConfig>,
    ) -> Self {
        let client = build_client(proxy.as_ref(), 720) // 12 分钟超时
            .expect("创建 HTTP 客户端失败");

        Self {
            token_manager,
            client,
        }
    }

    /// 获取 API 基础 URL
    #[allow(dead_code)]
    pub async fn base_url(&self) -> String {
        let region = {
            let tm = self.token_manager.lock().await;
            tm.config().region.clone()
        };
        format!(
            "https://q.{}.amazonaws.com/generateAssistantResponse",
            region
        )
    }

    /// 获取 API 基础域名
    #[allow(dead_code)]
    pub async fn base_domain(&self) -> String {
        let region = {
            let tm = self.token_manager.lock().await;
            tm.config().region.clone()
        };
        format!("q.{}.amazonaws.com", region)
    }

    /// 构建请求头
    fn build_headers(
        token: &str,
        credentials: &KiroCredentials,
        config: &crate::model::config::Config,
    ) -> anyhow::Result<HeaderMap> {
        let machine_id = machine_id::generate_from_credentials(credentials, config)
            .ok_or_else(|| anyhow::anyhow!("无法生成 machine_id，请检查凭证配置"))?;

        let kiro_version = config.kiro_version.clone();
        let os_name = config.system_version.clone();
        let node_version = config.node_version.clone();
        let base_domain = format!("q.{}.amazonaws.com", config.region);

        let x_amz_user_agent = format!("aws-sdk-js/1.0.27 KiroIDE-{}-{}", kiro_version, machine_id);

        let user_agent = format!(
            "aws-sdk-js/1.0.27 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererstreaming#1.0.27 m/E KiroIDE-{}-{}",
            os_name, node_version, kiro_version, machine_id
        );

        let mut headers = HeaderMap::new();

        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            "x-amzn-codewhisperer-optout",
            HeaderValue::from_static("true"),
        );
        headers.insert("x-amzn-kiro-agent-mode", HeaderValue::from_static("vibe"));
        headers.insert(
            "x-amz-user-agent",
            HeaderValue::from_str(&x_amz_user_agent).unwrap(),
        );
        headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_str(&user_agent).unwrap(),
        );
        headers.insert(HOST, HeaderValue::from_str(&base_domain).unwrap());
        headers.insert(
            "amz-sdk-invocation-id",
            HeaderValue::from_str(&Uuid::new_v4().to_string()).unwrap(),
        );
        headers.insert(
            "amz-sdk-request",
            HeaderValue::from_static("attempt=1; max=3"),
        );
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
        );
        headers.insert(CONNECTION, HeaderValue::from_static("close"));

        Ok(headers)
    }

    async fn acquire_token_snapshot(
        &self,
    ) -> anyhow::Result<(String, crate::model::config::Config, KiroCredentials)> {
        let mut tm = self.token_manager.lock().await;
        let token = tm.ensure_valid_token().await?;
        let config = tm.config().clone();
        let credentials = tm.credentials().clone();
        Ok((token, config, credentials))
    }

    /// 发送非流式 API 请求
    ///
    /// # Arguments
    /// * `request_body` - JSON 格式的请求体字符串
    ///
    /// # Returns
    /// 返回原始的 HTTP Response，不做解析
    pub async fn call_api(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        self.call_api_with_retry(request_body, false).await
    }

    /// 发送流式 API 请求
    ///
    /// # Arguments
    /// * `request_body` - JSON 格式的请求体字符串
    ///
    /// # Returns
    /// 返回原始的 HTTP Response，调用方负责处理流式数据
    pub async fn call_api_stream(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        self.call_api_with_retry(request_body, true).await
    }

    async fn call_api_with_retry(
        &self,
        request_body: &str,
        streaming: bool,
    ) -> anyhow::Result<reqwest::Response> {
        let body = request_body.to_string();
        let kind = if streaming { "流式" } else { "非流式" };
        let mut forced_refresh = false;

        for attempt in 1..=KIRO_MAX_ATTEMPTS {
            let (token, config, credentials) = self.acquire_token_snapshot().await?;
            let url = format!(
                "https://q.{}.amazonaws.com/generateAssistantResponse",
                config.region
            );
            let headers = Self::build_headers(&token, &credentials, &config)?;

            let response = match self
                .client
                .post(&url)
                .headers(headers)
                .body(body.clone())
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    if attempt < KIRO_MAX_ATTEMPTS && is_retryable_reqwest_error(&e) {
                        let delay = retry_backoff(attempt);
                        tracing::warn!(
                            "Kiro {} API 请求发送失败: {}，{:?} 后重试（{}/{}）",
                            kind,
                            e,
                            delay,
                            attempt,
                            KIRO_MAX_ATTEMPTS
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(e.into());
                }
            };

            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }

            let body_text = response.text().await.unwrap_or_default();

            if attempt < KIRO_MAX_ATTEMPTS && is_auth_status(status) && !forced_refresh {
                forced_refresh = true;
                tracing::warn!(
                    "{} API 返回 {}，将强制刷新 Token 后重试（{}/{}）",
                    kind,
                    status,
                    attempt,
                    KIRO_MAX_ATTEMPTS
                );
                let mut tm = self.token_manager.lock().await;
                tm.force_refresh().await?;
                continue;
            }

            if attempt < KIRO_MAX_ATTEMPTS && is_retryable_status(status) {
                let delay = retry_backoff(attempt);
                tracing::warn!(
                    "{} API 返回 {}，{:?} 后重试（{}/{}）",
                    kind,
                    status,
                    delay,
                    attempt,
                    KIRO_MAX_ATTEMPTS
                );
                tokio::time::sleep(delay).await;
                continue;
            }

            anyhow::bail!("{} API 请求失败: {} {}", kind, status, body_text);
        }

        unreachable!("attempt loop should return")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::model::config::Config;

    #[tokio::test]
    async fn test_base_url() {
        let config = Config::default();
        let credentials = KiroCredentials::default();
        let tm = TokenManager::new(config, credentials, None);
        let provider = KiroProvider::new(tm);
        let url = provider.base_url().await;
        assert!(url.contains("amazonaws.com"));
        assert!(url.contains("generateAssistantResponse"));
    }

    #[tokio::test]
    async fn test_base_domain() {
        let mut config = Config::default();
        config.region = "us-east-1".to_string();
        let credentials = KiroCredentials::default();
        let tm = TokenManager::new(config, credentials, None);
        let provider = KiroProvider::new(tm);
        assert_eq!(provider.base_domain().await, "q.us-east-1.amazonaws.com");
    }

    #[tokio::test]
    async fn test_build_headers() {
        let mut config = Config::default();
        config.region = "us-east-1".to_string();
        config.kiro_version = "0.8.0".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.profile_arn = Some("arn:aws:sso::123456789:profile/test".to_string());
        credentials.refresh_token = Some("a".repeat(150));

        let headers = KiroProvider::build_headers("test_token", &credentials, &config).unwrap();

        assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "application/json");
        assert_eq!(headers.get("x-amzn-codewhisperer-optout").unwrap(), "true");
        assert_eq!(headers.get("x-amzn-kiro-agent-mode").unwrap(), "vibe");
        assert!(headers
            .get(AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("Bearer "));
        assert_eq!(headers.get(CONNECTION).unwrap(), "close");
    }
}
