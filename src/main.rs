use anyhow::{Context, Result, anyhow};
use dialoguer::{Confirm, Input, Select, theme::ColorfulTheme};
use notify_rust::Notification;
use reqwest::{Client, ClientBuilder};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::time::{Duration, sleep};
use url::Url;

const CONFIG_PATH_ENV: &str = "DRONE_NOTIFY_CONFIG";

#[derive(Debug, Serialize, Deserialize, Default)]
struct ConfigFile {
    base_url: Option<String>,
    default_namespace: Option<String>,
    default_repo: Option<String>,
    default_branch: Option<String>,
    token: Option<String>,
    verify_ssl: Option<bool>,
    poll_interval_secs: Option<u64>,
    #[serde(default)]
    successful_targets: Vec<RepoTarget>,
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
    successful_targets: Vec<RepoTarget>,
}

#[derive(Debug, Deserialize)]
struct UserResponse {
    login: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
struct RepoTarget {
    namespace: String,
    repo: String,
    branch: String,
    #[serde(default)]
    success_count: u64,
    last_success_at: Option<u64>,
}

impl RepoTarget {
    fn new(
        namespace: impl Into<String>,
        repo: impl Into<String>,
        branch: impl Into<String>,
    ) -> Result<Self> {
        let namespace = namespace.into().trim().to_string();
        let repo = repo.into().trim().to_string();
        let branch = branch.into().trim().to_string();

        if namespace.is_empty() || repo.is_empty() || branch.is_empty() {
            return Err(anyhow!("namespace、repo、branch 不能为空"));
        }

        Ok(Self {
            namespace,
            repo,
            branch,
            success_count: 0,
            last_success_at: None,
        })
    }

    fn matches(&self, namespace: &str, repo: &str, branch: &str) -> bool {
        self.namespace == namespace && self.repo == repo && self.branch == branch
    }
}

#[derive(Debug, Deserialize)]
struct RepoResponse {
    namespace: Option<String>,
    name: Option<String>,
    slug: Option<String>,
    default_branch: Option<String>,
    active: Option<bool>,
}

#[derive(Debug, Clone)]
struct AccountRepo {
    namespace: String,
    repo: String,
    default_branch: Option<String>,
    active: Option<bool>,
}

impl RepoResponse {
    fn into_account_repo(self) -> Option<AccountRepo> {
        let (namespace, repo) = match (self.namespace, self.name, self.slug) {
            (Some(namespace), Some(name), _)
                if !namespace.trim().is_empty() && !name.trim().is_empty() =>
            {
                (namespace, name)
            }
            (_, _, Some(slug)) => {
                let (namespace, repo) = slug.split_once('/')?;
                (namespace.to_string(), repo.to_string())
            }
            _ => return None,
        };

        Some(AccountRepo {
            namespace: namespace.trim().to_string(),
            repo: repo.trim().to_string(),
            default_branch: self
                .default_branch
                .map(|branch| branch.trim().to_string())
                .filter(|branch| !branch.is_empty()),
            active: self.active,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BranchResponse {
    Name(String),
    Object { name: Option<String> },
}

impl BranchResponse {
    fn into_name(self) -> Option<String> {
        match self {
            BranchResponse::Name(name) => {
                let name = name.trim().to_string();
                (!name.is_empty()).then_some(name)
            }
            BranchResponse::Object { name } => name
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum RepoSelectionMode {
    Manual,
    History,
    Account,
}

#[derive(Debug, Deserialize, Clone)]
struct BuildResponse {
    number: u64,
    status: Option<String>,
    #[serde(rename = "link", alias = "link_url")]
    link_url: Option<String>,
    message: Option<String>,
    after: Option<String>,
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
    let config_path = resolve_config_path()?;

    let config = load_or_init_config(&config_path)?;
    let client = build_client(&config)?;

    let user = fetch_user(&client, &config).await?;
    println!("Hello {}", user.login);

    let target = select_repo_target(&client, &config, &user.login).await?;
    let namespace = target.namespace;
    let repo = target.repo;
    let branch = target.branch;
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

    if is_success_status(final_build.status.as_deref()) {
        if let Err(err) = record_successful_target(&config_path, &namespace, &repo, &branch) {
            eprintln!("记录成功组合失败：{}", err);
        }
    }

    send_notification(&final_build, &config, &namespace, &repo)?;
    Ok(())
}

fn resolve_config_path() -> Result<PathBuf> {
    if let Ok(path) = env::var(CONFIG_PATH_ENV) {
        let path = path.trim();
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }

    env::current_exe()
        .context("获取当前所在目录失败")?
        .parent()
        .map(|p| p.join("config.toml"))
        .ok_or_else(|| anyhow!("无法获取当前所在目录"))
}

fn load_or_init_config(path: &Path) -> Result<Config> {
    let mut cfg = read_config_file(path)?;

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
        successful_targets: cfg.successful_targets.clone(),
    };

    write_config_file(path, &cfg)?;

    Ok(config)
}

fn read_config_file(path: &Path) -> Result<ConfigFile> {
    if path.exists() {
        let content = fs::read_to_string(path).context("读取配置文件失败")?;
        toml::from_str(&content).context("解析配置文件失败")
    } else {
        Ok(ConfigFile::default())
    }
}

fn write_config_file(path: &Path, cfg: &ConfigFile) -> Result<()> {
    let serialized = toml::to_string_pretty(cfg).context("序列化配置失败")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("创建配置目录失败")?;
    }
    fs::write(path, serialized).context("写入配置文件失败")?;

    Ok(())
}

fn record_successful_target(path: &Path, namespace: &str, repo: &str, branch: &str) -> Result<()> {
    let mut cfg = read_config_file(path)?;
    let now = current_unix_timestamp()?;
    let target = RepoTarget::new(namespace.to_string(), repo.to_string(), branch.to_string())?;

    if let Some(existing) = cfg
        .successful_targets
        .iter_mut()
        .find(|item| item.matches(&target.namespace, &target.repo, &target.branch))
    {
        existing.success_count = existing.success_count.saturating_add(1);
        existing.last_success_at = Some(now);
    } else {
        cfg.successful_targets.push(RepoTarget {
            namespace: target.namespace,
            repo: target.repo,
            branch: target.branch,
            success_count: 1,
            last_success_at: Some(now),
        });
    }

    cfg.successful_targets.sort_by(|a, b| {
        b.last_success_at
            .cmp(&a.last_success_at)
            .then_with(|| a.namespace.cmp(&b.namespace))
            .then_with(|| a.repo.cmp(&b.repo))
            .then_with(|| a.branch.cmp(&b.branch))
    });
    cfg.successful_targets.truncate(50);
    write_config_file(path, &cfg)
}

fn current_unix_timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("获取当前时间失败")?
        .as_secs())
}

fn is_success_status(status: Option<&str>) -> bool {
    status
        .map(|status| status.eq_ignore_ascii_case("success"))
        .unwrap_or(false)
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

async fn select_repo_target(
    client: &Client,
    config: &Config,
    account_login: &str,
) -> Result<RepoTarget> {
    let theme = ColorfulTheme::default();
    let mut modes = vec![
        ("手动输入 namespace/repo/branch", RepoSelectionMode::Manual),
        ("从账号名下获取仓库", RepoSelectionMode::Account),
    ];
    let default_mode = if config.successful_targets.is_empty() {
        0
    } else {
        modes.insert(1, ("从过往成功记录选择", RepoSelectionMode::History));
        1
    };

    let labels: Vec<&str> = modes.iter().map(|(label, _)| *label).collect();
    let selected = Select::with_theme(&theme)
        .with_prompt("选择 namespace/repo/branch 的输入方式")
        .items(&labels)
        .default(default_mode)
        .interact()?;

    match modes[selected].1 {
        RepoSelectionMode::Manual => prompt_repo_target_manual(config),
        RepoSelectionMode::History => prompt_repo_target_from_history(config),
        RepoSelectionMode::Account => {
            prompt_repo_target_from_account(client, config, account_login).await
        }
    }
}

fn prompt_repo_target_manual(config: &Config) -> Result<RepoTarget> {
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

    RepoTarget::new(namespace, repo, branch)
}

fn prompt_repo_target_from_history(config: &Config) -> Result<RepoTarget> {
    let theme = ColorfulTheme::default();
    let labels: Vec<String> = config
        .successful_targets
        .iter()
        .map(format_target_history_label)
        .collect();

    let selected = Select::with_theme(&theme)
        .with_prompt("选择过往成功执行过的组合")
        .items(&labels)
        .default(0)
        .interact()?;

    Ok(config.successful_targets[selected].clone())
}

async fn prompt_repo_target_from_account(
    client: &Client,
    config: &Config,
    account_login: &str,
) -> Result<RepoTarget> {
    let theme = ColorfulTheme::default();
    let repos = fetch_user_repos(client, config).await?;
    let mut account_repos: Vec<AccountRepo> = repos
        .iter()
        .filter(|repo| repo.namespace == account_login)
        .cloned()
        .collect();

    if account_repos.is_empty() {
        println!(
            "未找到 namespace 为 {} 的仓库，将展示当前账号可访问的仓库",
            account_login
        );
        account_repos = repos;
    }

    if account_repos.is_empty() {
        return Err(anyhow!("当前账号没有可选择的仓库"));
    }

    account_repos.sort_by(|a, b| {
        a.namespace
            .cmp(&b.namespace)
            .then_with(|| a.repo.cmp(&b.repo))
    });

    let labels: Vec<String> = account_repos
        .iter()
        .map(format_account_repo_label)
        .collect();
    let selected = Select::with_theme(&theme)
        .with_prompt("选择仓库")
        .items(&labels)
        .default(0)
        .interact()?;
    let selected_repo = account_repos[selected].clone();

    let branch = match fetch_repo_branches(
        client,
        config,
        &selected_repo.namespace,
        &selected_repo.repo,
    )
    .await
    {
        Ok(branches) if !branches.is_empty() => {
            select_branch_from_list(config, &selected_repo, branches)?
        }
        Ok(_) => prompt_branch_manually(config, selected_repo.default_branch.as_deref())?,
        Err(err) => {
            println!("获取分支列表失败，将手动输入 branch：{}", err);
            prompt_branch_manually(config, selected_repo.default_branch.as_deref())?
        }
    };

    RepoTarget::new(selected_repo.namespace, selected_repo.repo, branch)
}

fn select_branch_from_list(
    config: &Config,
    repo: &AccountRepo,
    branches: Vec<String>,
) -> Result<String> {
    let theme = ColorfulTheme::default();
    let default_branch = repo
        .default_branch
        .as_deref()
        .unwrap_or(config.default_branch.as_str());
    let default_index = branches
        .iter()
        .position(|branch| branch == default_branch)
        .unwrap_or(0);

    let selected = Select::with_theme(&theme)
        .with_prompt("选择 branch")
        .items(&branches)
        .default(default_index)
        .interact()?;

    Ok(branches[selected].clone())
}

fn prompt_branch_manually(config: &Config, repo_default_branch: Option<&str>) -> Result<String> {
    let theme = ColorfulTheme::default();
    let default_branch = repo_default_branch
        .filter(|branch| !branch.trim().is_empty())
        .unwrap_or(config.default_branch.as_str());

    let branch: String = Input::with_theme(&theme)
        .with_prompt("branch")
        .default(default_branch.to_string())
        .interact_text()?;

    let target = RepoTarget::new(
        config.default_namespace.clone(),
        config.default_repo.clone(),
        branch,
    )?;

    Ok(target.branch)
}

async fn fetch_user_repos(client: &Client, config: &Config) -> Result<Vec<AccountRepo>> {
    let mut url = Url::parse(&config.base_url).context("解析 baseUrl 失败")?;
    url.set_path("api/user/repos");

    let res = client
        .get(url)
        .bearer_auth(&config.token)
        .send()
        .await
        .context("获取账号仓库列表失败")?
        .error_for_status()
        .context("获取账号仓库列表非 2xx 响应")?;

    let repos: Vec<RepoResponse> = res.json().await.context("解析账号仓库列表失败")?;
    let repos = repos
        .into_iter()
        .filter_map(RepoResponse::into_account_repo)
        .collect();

    Ok(repos)
}

async fn fetch_repo_branches(
    client: &Client,
    config: &Config,
    namespace: &str,
    repo: &str,
) -> Result<Vec<String>> {
    let mut url = Url::parse(&config.base_url).context("解析 baseUrl 失败")?;
    url.set_path(&format!("api/repos/{}/{}/branches", namespace, repo));

    let res = client
        .get(url)
        .bearer_auth(&config.token)
        .send()
        .await
        .context("获取分支列表失败")?
        .error_for_status()
        .context("获取分支列表非 2xx 响应")?;

    let branches: Vec<BranchResponse> = res.json().await.context("解析分支列表失败")?;
    let mut branches: Vec<String> = branches
        .into_iter()
        .filter_map(BranchResponse::into_name)
        .collect();
    branches.sort();
    branches.dedup();

    Ok(branches)
}

fn format_target_history_label(target: &RepoTarget) -> String {
    if target.success_count > 0 {
        format!(
            "{}/{} @ {} (成功 {} 次)",
            target.namespace, target.repo, target.branch, target.success_count
        )
    } else {
        format!("{}/{} @ {}", target.namespace, target.repo, target.branch)
    }
}

fn format_account_repo_label(repo: &AccountRepo) -> String {
    match repo.active {
        Some(false) => format!("{}/{} (未激活)", repo.namespace, repo.repo),
        _ => format!("{}/{}", repo.namespace, repo.repo),
    }
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
