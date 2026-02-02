use anyhow::{Context, Result, anyhow};
use dialoguer::{Confirm, Input, theme::ColorfulTheme};
use notify_rust::Notification;
use reqwest::{Client, ClientBuilder};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, path::Path};
use tokio::time::{Duration, sleep};
use url::Url;

#[derive(Debug, Serialize, Deserialize, Default)]
struct ConfigFile {
    base_url: Option<String>,
    default_namespace: Option<String>,
    default_repo: Option<String>,
    default_branch: Option<String>,
    token: Option<String>,
    verify_ssl: Option<bool>,
    poll_interval_secs: Option<u64>,
}

#[derive(Debug, Clone)]
struct Config {
    base_url: String,
    default_namespace: String,
    default_repo: String,
    default_branch: String,
    token: String,
    verify_ssl: bool,
    poll_interval_secs: u64,
}

#[derive(Debug, Deserialize)]
struct UserResponse {
    login: String,
}

#[derive(Debug, Deserialize, Clone)]
struct BuildResponse {
    number: u64,
    status: Option<String>,
    #[serde(rename = "link", alias = "link_url")]
    link_url: Option<String>,
    message: Option<String>,
    after: Option<String>,
    before: Option<String>,
    #[serde(rename = "ref")]
    git_ref: Option<String>,
    source: Option<String>,
    target: Option<String>,
    author_login: Option<String>,
    author_name: Option<String>,
    author_email: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = std::env::current_exe()
        .context("获取当前所在目录失败")?
        .parent()
        .map(|p| p.join("config.toml"))
        .ok_or_else(|| anyhow!("无法获取当前所在目录"))?;

    let config = load_or_init_config(&config_path)?;
    let client = build_client(&config)?;

    let user = fetch_user(&client, &config).await?;
    println!("Hello {}", user.login);

    let (namespace, repo, branch) = prompt_repo_info(&config)?;
    let theme = ColorfulTheme::default();

    let commit_preview =
        match fetch_latest_branch_build(&client, &config, &namespace, &repo, &branch).await {
            Ok(build) => Some(build),
            Err(err) => {
                println!(
                    "未能获取分支 {} 的 commit 信息，将询问后再决定是否继续：{}",
                    branch, err
                );
                None
            }
        };

    if let Some(build) = commit_preview {
        print_commit_preview(&build, &branch);
        let confirmed = Confirm::with_theme(&theme)
            .with_prompt("上述 commit 是否正确，确认后才会触发构建")
            .default(true)
            .interact()?;
        if !confirmed {
            println!("已取消触发构建");
            return Ok(());
        }
    } else {
        let proceed_without_preview = Confirm::with_theme(&theme)
            .with_prompt("未获取到 commit 信息，仍要触发构建吗？")
            .default(false)
            .interact()?;
        if !proceed_without_preview {
            println!("已取消触发构建");
            return Ok(());
        }
    }

    let build = trigger_build(&client, &config, &namespace, &repo, &branch).await?;
    println!("触发构建 #{}, 分支 {}", build.number, branch);

    let final_build = poll_build(&client, &config, &namespace, &repo, build.number).await?;
    let status = final_build
        .status
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    println!("构建 #{} 结束，状态：{}", final_build.number, status);

    send_notification(&final_build, &config, &namespace, &repo)?;
    Ok(())
}

fn load_or_init_config(path: &Path) -> Result<Config> {
    let mut cfg: ConfigFile = if path.exists() {
        let content = fs::read_to_string(path).context("读取配置文件失败")?;
        toml::from_str(&content).context("解析配置文件失败")?
    } else {
        ConfigFile::default()
    };

    let theme = ColorfulTheme::default();

    if cfg
        .base_url
        .as_deref()
        .map(str::trim)
        .map(|s| s.is_empty())
        .unwrap_or(true)
    {
        cfg.base_url = Some(
            Input::with_theme(&theme)
                .with_prompt("Drone baseUrl (例如 https://drone.example.com)")
                .interact_text()?,
        );
    }

    if cfg
        .default_namespace
        .as_deref()
        .map(str::trim)
        .map(|s| s.is_empty())
        .unwrap_or(true)
    {
        cfg.default_namespace = Some(
            Input::with_theme(&theme)
                .with_prompt("默认 namespace")
                .interact_text()?,
        );
    }

    if cfg
        .default_repo
        .as_deref()
        .map(str::trim)
        .map(|s| s.is_empty())
        .unwrap_or(true)
    {
        cfg.default_repo = Some(
            Input::with_theme(&theme)
                .with_prompt("默认 repo")
                .interact_text()?,
        );
    }

    if cfg
        .default_branch
        .as_deref()
        .map(str::trim)
        .map(|s| s.is_empty())
        .unwrap_or(true)
    {
        cfg.default_branch = Some(
            Input::with_theme(&theme)
                .with_prompt("默认 branch")
                .default("main".to_string())
                .interact_text()?,
        );
    }

    if cfg
        .token
        .as_deref()
        .map(str::trim)
        .map(|s| s.is_empty())
        .unwrap_or(true)
    {
        cfg.token = Some(
            Input::with_theme(&theme)
                .with_prompt("Drone token")
                .interact_text()?,
        );
    }

    if cfg.verify_ssl.is_none() {
        cfg.verify_ssl = Some(
            Confirm::with_theme(&theme)
                .with_prompt("验证 SSL 证书？(Yes 验证 / No 跳过验证)")
                .default(true)
                .interact()?,
        );
    }

    if cfg.poll_interval_secs.is_none() {
        cfg.poll_interval_secs = Some(
            Input::with_theme(&theme)
                .with_prompt("轮询间隔秒数")
                .default(3)
                .interact_text()?,
        );
    }

    let config = Config {
        base_url: clean_base_url(cfg.base_url.clone().unwrap()),
        default_namespace: cfg.default_namespace.clone().unwrap(),
        default_repo: cfg.default_repo.clone().unwrap(),
        default_branch: cfg.default_branch.clone().unwrap(),
        token: cfg.token.clone().unwrap(),
        verify_ssl: cfg.verify_ssl.unwrap(),
        poll_interval_secs: cfg.poll_interval_secs.unwrap(),
    };

    let serialized = toml::to_string_pretty(&cfg).context("序列化配置失败")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("创建配置目录失败")?;
    }
    fs::write(path, serialized).context("写入配置文件失败")?;

    Ok(config)
}

fn build_client(config: &Config) -> Result<Client> {
    let mut builder = ClientBuilder::new()
        .user_agent("drone-cli-helper/0.1")
        .timeout(Duration::from_secs(30));

    if !config.verify_ssl {
        builder = builder.danger_accept_invalid_certs(true);
    }

    builder.build().context("构建 HTTP 客户端失败")
}

async fn fetch_user(client: &Client, config: &Config) -> Result<UserResponse> {
    let mut url = Url::parse(&config.base_url).context("解析 baseUrl 失败")?;
    url.set_path("api/user");

    let res = client
        .get(url)
        .bearer_auth(&config.token)
        .send()
        .await
        .context("请求 /api/user 失败")?
        .error_for_status()
        .context("获取 /api/user 非 2xx 响应")?;

    Ok(res
        .json::<UserResponse>()
        .await
        .context("解析用户响应失败")?)
}

fn prompt_repo_info(config: &Config) -> Result<(String, String, String)> {
    let theme = ColorfulTheme::default();
    let namespace: String = Input::with_theme(&theme)
        .with_prompt("namespace")
        .default(config.default_namespace.clone())
        .interact_text()?;

    let repo: String = Input::with_theme(&theme)
        .with_prompt("repo")
        .default(config.default_repo.clone())
        .interact_text()?;

    let branch: String = Input::with_theme(&theme)
        .with_prompt("branch")
        .default(config.default_branch.clone())
        .interact_text()?;

    Ok((namespace, repo, branch))
}

async fn trigger_build(
    client: &Client,
    config: &Config,
    namespace: &str,
    repo: &str,
    branch: &str,
) -> Result<BuildResponse> {
    let mut url = Url::parse(&config.base_url).context("解析 baseUrl 失败")?;
    url.set_path(&format!("api/repos/{}/{}/builds", namespace, repo));
    url.query_pairs_mut().append_pair("branch", branch);

    let res = client
        .post(url)
        .bearer_auth(&config.token)
        .send()
        .await
        .context("触发构建请求失败")?
        .error_for_status()
        .context("触发构建非 2xx 响应")?;

    Ok(res
        .json::<BuildResponse>()
        .await
        .context("解析构建创建响应失败")?)
}

async fn fetch_latest_branch_build(
    client: &Client,
    config: &Config,
    namespace: &str,
    repo: &str,
    branch: &str,
) -> Result<BuildResponse> {
    let mut url = Url::parse(&config.base_url).context("解析 baseUrl 失败")?;
    url.set_path(&format!("api/repos/{}/{}/builds", namespace, repo));
    url.query_pairs_mut()
        .append_pair("page", "1")
        .append_pair("per_page", "50");

    let res = client
        .get(url)
        .bearer_auth(&config.token)
        .send()
        .await
        .context("获取构建列表失败")?
        .error_for_status()
        .context("获取构建列表非 2xx 响应")?;

    let builds: Vec<BuildResponse> = res.json().await.context("解析构建列表失败")?;

    builds
        .into_iter()
        .find(|build| {
            build
                .git_ref
                .as_deref()
                .and_then(|r| r.rsplit('/').next())
                .map(|r| r == branch)
                .unwrap_or(false)
                || build
                    .target
                    .as_deref()
                    .map(|r| r == branch)
                    .unwrap_or(false)
                || build
                    .source
                    .as_deref()
                    .map(|r| r == branch)
                    .unwrap_or(false)
        })
        .ok_or_else(|| anyhow!("未在最近的构建列表中找到分支 {} 的记录", branch))
}

fn print_commit_preview(build: &BuildResponse, branch: &str) {
    println!("即将触发分支 {} 的构建，最近找到的 commit 信息：", branch);
    if let Some(sha) = &build.after {
        println!("  commit: {}", sha);
    }
    if let Some(msg) = &build.message {
        println!("  message: {}", msg);
    }
    if let Some(author) = build.author_name.as_ref().or(build.author_login.as_ref()) {
        match build.author_email.as_ref() {
            Some(email) => println!("  author: {} <{}>", author, email),
            None => println!("  author: {}", author),
        }
    }
    if let Some(reference) = &build.git_ref {
        println!("  ref: {}", reference);
    }
    if let Some(link) = &build.link_url {
        println!("  link: {}", link);
    }
}

async fn poll_build(
    client: &Client,
    config: &Config,
    namespace: &str,
    repo: &str,
    build_number: u64,
) -> Result<BuildResponse> {
    let final_states: HashSet<&str> = ["success", "failure", "killed", "canceled", "cancelled"]
        .into_iter()
        .collect();

    loop {
        let mut url = Url::parse(&config.base_url).context("解析 baseUrl 失败")?;
        url.set_path(&format!(
            "api/repos/{}/{}/builds/{}",
            namespace, repo, build_number
        ));

        let res = client
            .get(url.clone())
            .bearer_auth(&config.token)
            .send()
            .await
            .with_context(|| format!("轮询构建 {} 请求失败", build_number))?
            .error_for_status()
            .context("轮询构建返回非 2xx")?;

        let build: BuildResponse = res.json().await.context("解析构建状态失败")?;

        if let Some(status) = build.status.as_deref() {
            let status_lc = status.to_lowercase();
            if final_states.contains(status_lc.as_str()) {
                return Ok(build);
            }
        }

        sleep(Duration::from_secs(config.poll_interval_secs)).await;
    }
}

fn send_notification(build: &BuildResponse, config: &Config, ns: &str, repo: &str) -> Result<()> {
    let status = build.status.as_deref().unwrap_or("unknown").to_string();
    let summary = format!("Drone 构建 {}", status);
    let mut body = format!("{}/{} #{}, status: {}", ns, repo, build.number, status);

    if let Some(msg) = &build.message {
        body.push_str(&format!("\n{}", msg));
    }

    let link = build.link_url.clone().unwrap_or_else(|| {
        format!(
            "{}/api/repos/{}/{}/builds/{}",
            config.base_url, ns, repo, build.number
        )
    });

    Notification::new()
        .summary(&summary)
        .body(&format!("{}\n{}", body, link))
        .show()
        .map(|_| ())
        .map_err(|e| anyhow!("发送系统通知失败: {}", e))
}

fn clean_base_url(raw: String) -> String {
    let mut url = raw.trim().trim_end_matches('/').to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        url = format!("https://{}", url);
    }
    url
}
