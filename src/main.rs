use anyhow::Result;
use reqwest::Client;
use rmcp::model::{
    ServerCapabilities, ServerInfo,
};
use rmcp::service::ServiceExt;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::ServerHandler;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::info;
use url::Url;

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36";
const DDG_SEARCH_URL: &str = "https://html.duckduckgo.com/html";
const SEARCH_RATE_LIMIT: usize = 30;
const FETCH_RATE_LIMIT: usize = 20;
const WINDOW_SECS: u64 = 60;
const DDG_UDDG_PREFIX: &str = "//duckduckgo.com/l/?uddg=";
const JS_TRACKER_FILTER: &str = "y.js";

static WHITESPACE_RE: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| regex::Regex::new(r"\s+").expect("invalid regex"));

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub link: String,
    pub snippet: String,
    pub position: usize,
}

#[derive(Debug)]
pub struct RateLimiter {
    requests: Vec<Instant>,
    max_requests: usize,
}

impl RateLimiter {
    pub fn new(max_requests: usize) -> Self {
        Self {
            requests: Vec::new(),
            max_requests,
        }
    }

    pub async fn acquire(&mut self) {
        loop {
            let now = Instant::now();
            self.requests.retain(|&req| now.duration_since(req) < Duration::from_secs(WINDOW_SECS));

            if self.requests.len() < self.max_requests {
                self.requests.push(now);
                return;
            }

            if let Some(oldest) = self.requests.first() {
                let wait = Duration::from_secs(WINDOW_SECS) - now.duration_since(*oldest);
                if wait > Duration::ZERO {
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct DuckDuckGoSearcher {
    client: Client,
}

impl DuckDuckGoSearcher {
    pub fn new() -> Self {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        Self { client }
    }

    pub async fn search(
        &self,
        query: &str,
        max_results: usize,
        region: &str,
        safe_search: &str,
    ) -> Result<String> {
        let mut form_data: Vec<(&str, &str)> = vec![
            ("q", query),
            ("b", ""),
        ];

        if !region.is_empty() {
            form_data.push(("kl", region));
        }

        form_data.push(("kp", safe_search));

        info!("Searching DuckDuckGo for: {}", query);

        let response = self.client
            .post(DDG_SEARCH_URL)
            .form(&form_data)
            .send()
            .await?;

        let body = response.text().await?;
        let results = self.parse_search_results(&body, max_results)?;
        let formatted = Self::format_results_for_llm(&results);

        Ok(formatted)
    }

    fn parse_search_results(&self, html: &str, max_results: usize) -> Result<Vec<SearchResult>> {
        let document = Html::parse_document(html);
        let results_selector = Selector::parse(".result").map_err(|e| anyhow::anyhow!("Invalid selector: {}", e))?;
        let title_selector = Selector::parse(".result__title").map_err(|e| anyhow::anyhow!("Invalid selector: {}", e))?;
        let link_selector = Selector::parse("a").map_err(|e| anyhow::anyhow!("Invalid selector: {}", e))?;
        let snippet_selector = Selector::parse(".result__snippet").map_err(|e| anyhow::anyhow!("Invalid selector: {}", e))?;

        let mut results = Vec::new();

        for (idx, result) in document.select(&results_selector).enumerate() {
            if idx >= max_results {
                break;
            }

            let title_elem = result.select(&title_selector).next();

            let (title, link) = match title_elem {
                Some(elem) => {
                    let link_elem = match elem.select(&link_selector).next() {
                        Some(l) => l,
                        None => continue,
                    };
                    let title = link_elem.text().collect::<String>().trim().to_string();
                    let link = link_elem.value().attr("href").unwrap_or("").to_string();
                    (title, link)
                }
                None => continue,
            };

            if link.contains(JS_TRACKER_FILTER) {
                continue;
            }

            let link = if link.starts_with(DDG_UDDG_PREFIX) {
                let clean = link.trim_start_matches(DDG_UDDG_PREFIX);
                if let Ok(decoded) = Url::parse(&format!("https://{}", clean)) {
                    decoded.query_pairs().find(|(k, _)| k == "uddg")
                        .map(|(_, v)| v.to_string())
                        .unwrap_or(link)
                } else {
                    link
                }
            } else {
                link
            };

            let snippet = result.select(&snippet_selector)
                .next()
                .map(|elem| elem.text().collect::<String>().trim().to_string())
                .unwrap_or_default();

            results.push(SearchResult {
                title,
                link,
                snippet,
                position: results.len() + 1,
            });
        }

        Ok(results)
    }

    fn format_results_for_llm(results: &[SearchResult]) -> String {
        if results.is_empty() {
            return "No results were found for your search query. This could be due to DuckDuckGo's bot detection or the query returned no matches. Please try rephrasing your search or try again in a few minutes.".to_string();
        }

        let mut output = format!("Found {} search results:\n\n", results.len());

        for result in results {
            output.push_str(&format!("{}. {}\n", result.position, result.title));
            output.push_str(&format!("   URL: {}\n", result.link));
            output.push_str(&format!("   Summary: {}\n\n", result.snippet));
        }

        output
    }

    pub async fn fetch_content(
        &self,
        url: &str,
        start_index: usize,
        max_length: usize,
    ) -> Result<String> {
        info!("Fetching content from: {}", url);

        let response = self.client
            .get(url)
            .header("User-Agent", USER_AGENT)
            .send()
            .await?;

        let body = response.text().await?;
        let text = Self::extract_text(&body, start_index, max_length)?;

        Ok(text)
    }

    fn extract_text(html: &str, start_index: usize, max_length: usize) -> Result<String> {
        let document = Html::parse_document(html);
        let text = document.root_element().text().collect::<String>();
        let cleaned = WHITESPACE_RE.replace_all(&text, " ").trim().to_string();

        let total_length = cleaned.len();

        let truncated = if start_index >= total_length {
            String::new()
        } else {
            cleaned.chars().skip(start_index).take(max_length).collect()
        };

        let is_truncated = start_index + max_length < total_length;

        let mut result = truncated;
        let end_pos = start_index + result.len();

        result.push_str(&format!(
            "\n\n---\n[Content info: Showing characters {}-{} of {} total",
            start_index, end_pos, total_length
        ));

        if is_truncated {
            result.push_str(&format!(". Use start_index={} to see more", start_index + max_length));
        }
        result.push_str("]");

        Ok(result)
    }
}

pub enum SafeSearchMode {
    Strict,   // kp=1
    Moderate, // kp=-1
    Off,      // kp=-2
}

impl SafeSearchMode {
    pub fn from_str(s: &str) -> Self {
        match s.to_uppercase().as_str() {
            "STRICT" => Self::Strict,
            "OFF" => Self::Off,
            _ => Self::Moderate,
        }
    }

    pub fn as_kp_value(&self) -> &'static str {
        match self {
            Self::Strict => "1",
            Self::Moderate => "-1",
            Self::Off => "-2",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DuckDuckGoHandler {
    searcher: Arc<Mutex<DuckDuckGoSearcher>>,
    search_limiter: Arc<Mutex<RateLimiter>>,
    fetch_limiter: Arc<Mutex<RateLimiter>>,
}

impl DuckDuckGoHandler {
    pub fn new() -> Self {
        Self {
            searcher: Arc::new(Mutex::new(DuckDuckGoSearcher::new())),
            search_limiter: Arc::new(Mutex::new(RateLimiter::new(SEARCH_RATE_LIMIT))),
            fetch_limiter: Arc::new(Mutex::new(RateLimiter::new(FETCH_RATE_LIMIT))),
        }
    }
}

impl Default for DuckDuckGoHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerHandler for DuckDuckGoHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("DuckDuckGo MCP Server - Search the web and fetch webpage content")
            .with_server_info(rmcp::model::Implementation::new("duckduckgo-mcp-server", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        use rmcp::model::JsonObject;
        use schemars::schema_for;

        let search_schema: JsonObject = serde_json::from_value(serde_json::to_value(&schema_for!(SearchParams)).unwrap()).unwrap();
        let fetch_schema: JsonObject = serde_json::from_value(serde_json::to_value(&schema_for!(FetchContentParams)).unwrap()).unwrap();

        let tools = vec![
            rmcp::model::Tool::new_with_raw("search", Some("Search the web using DuckDuckGo.".into()), search_schema),
            rmcp::model::Tool::new_with_raw("fetch_content", Some("Fetch and extract content from a webpage.".into()), fetch_schema),
        ];

        Ok(rmcp::model::ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let name = request.name.as_ref();
        let args = request.arguments.unwrap_or_default();

        match name {
            "search" => {
                let params: SearchParams = serde_json::from_value(serde_json::Value::Object(args))
                    .map_err(|e| rmcp::ErrorData::invalid_params(std::borrow::Cow::Owned(format!("{}", e)), None))?;

                let safe_search = std::env::var("DDG_SAFE_SEARCH")
                    .unwrap_or_else(|_| "MODERATE".to_string());
                let max_results = params.max_results.unwrap_or(10).clamp(1, 20);
                let region = params.region.unwrap_or_default();

                {
                    let mut limiter = self.search_limiter.lock().await;
                    limiter.acquire().await;
                }

                let searcher = self.searcher.lock().await;
                let result = searcher.search(&params.query, max_results, &region, &SafeSearchMode::from_str(&safe_search).as_kp_value()).await;

                match result {
                    Ok(text) => Ok(rmcp::model::CallToolResult::success(vec![
                        rmcp::model::Content::text(text)
                    ])),
                    Err(e) => Ok(rmcp::model::CallToolResult::error(vec![
                        rmcp::model::Content::text(format!("Search error: {}", e))
                    ])),
                }
            }
            "fetch_content" => {
                let params: FetchContentParams = serde_json::from_value(serde_json::Value::Object(args))
                    .map_err(|e| rmcp::ErrorData::invalid_params(std::borrow::Cow::Owned(format!("{}", e)), None))?;

                if !params.url.starts_with("http://") && !params.url.starts_with("https://") {
                    return Ok(rmcp::model::CallToolResult::error(vec![
                        rmcp::model::Content::text("URL must start with http:// or https://")
                    ]));
                }

                let start_index = params.start_index.unwrap_or(0);
                let max_length = params.max_length.unwrap_or(8000);

                {
                    let mut limiter = self.fetch_limiter.lock().await;
                    limiter.acquire().await;
                }

                let searcher = self.searcher.lock().await;
                let result = searcher.fetch_content(&params.url, start_index, max_length).await;

                match result {
                    Ok(text) => Ok(rmcp::model::CallToolResult::success(vec![
                        rmcp::model::Content::text(text)
                    ])),
                    Err(e) => Ok(rmcp::model::CallToolResult::error(vec![
                        rmcp::model::Content::text(format!("Fetch error: {}", e))
                    ])),
                }
            }
            _ => Err(rmcp::ErrorData::method_not_found::<rmcp::model::CallToolRequestMethod>())
        }
    }
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct SearchParams {
    /// The search query string. Be specific for better results.
    pub query: String,
    /// Maximum number of results to return, between 1 and 20 (default: 10).
    #[serde(default)]
    pub max_results: Option<usize>,
    /// Optional region/language code (e.g., 'us-en', 'cn-zh', 'jp-ja', 'wt-wt').
    #[serde(default)]
    pub region: Option<String>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct FetchContentParams {
    /// The full URL of the webpage to fetch.
    pub url: String,
    /// Character offset to start reading from (default: 0).
    #[serde(default)]
    pub start_index: Option<usize>,
    /// Maximum number of characters to return (default: 8000).
    #[serde(default)]
    pub max_length: Option<usize>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    info!("Starting DuckDuckGo MCP Server");

    let safe_search = std::env::var("DDG_SAFE_SEARCH")
        .unwrap_or_else(|_| "MODERATE".to_string());
    let region = std::env::var("DDG_REGION").unwrap_or_default();

    info!("SafeSearch: {}", safe_search);
    info!("Default Region: {}", region);

    let transport_type = std::env::var("MCP_TRANSPORT")
        .unwrap_or_else(|_| "stdio".to_string());

    let handler = DuckDuckGoHandler::new();

    match transport_type.as_str() {
        "stdio" => {
            info!("Using stdio transport");
            let service = handler.serve(rmcp::transport::io::stdio());
            service.await?;
        }
        "streamable-http" | "http" => {
            info!("Using streamable-http transport");

            let host = std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
            let port = std::env::var("PORT")
                .unwrap_or_else(|_| "8080".to_string())
                .parse::<u16>()
                .unwrap_or(8080);

            let config = StreamableHttpServerConfig::default()
                .with_stateful_mode(false)
                .with_json_response(true);

            let service: StreamableHttpService<DuckDuckGoHandler, LocalSessionManager> =
                StreamableHttpService::new(move || Ok(handler.clone()), Default::default(), config);

            let router = axum::Router::new().nest_service("/mcp", service);
            let addr = format!("{}:{}", host, port);
            info!("Listening on {}", addr);

            let listener = tokio::net::TcpListener::bind(&addr).await?;
            axum::serve(listener, router).await?;
        }
        _ => {
            tracing::warn!("Unknown transport '{}', only stdio and streamable-http are supported", transport_type);
        }
    }

    Ok(())
}
