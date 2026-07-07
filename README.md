# drone_notify

一个 Drone 构建触发工具。

它会触发指定仓库分支的构建。
触发前会展示最近一次 commit 信息。
构建结束后会发送系统通知。

## 功能

- 支持手动输入 `namespace`、`repo`、`branch`。
- 支持从过往成功记录中选择。
- 支持从当前账号名下获取仓库。
- 支持成功后记录常用组合。
- 支持自定义 `config.toml` 路径。

## 运行

在项目目录运行：

```bash
cargo run
```

在任意目录运行：

```bash
cargo run --manifest-path /Users/colour93/Code/drone-notify/Cargo.toml
```

## 指定配置文件

使用 `DRONE_NOTIFY_CONFIG` 指定配置文件路径。

```bash
DRONE_NOTIFY_CONFIG=~/Scripts/config.toml cargo run --manifest-path /Users/colour93/Code/drone-notify/Cargo.toml
```

不指定时，程序会使用程序所在目录下的 `config.toml`。

## 配置

首次运行会交互式生成配置。

```toml
base_url = "https://drone.example.com"
default_namespace = "your-name"
default_repo = "your-repo"
default_branch = "main"
token = "your-drone-token"
verify_ssl = true
poll_interval_secs = 3
```

## 成功记录

当构建状态为 `success` 时，会写入当前使用的 `config.toml`。

```toml
[[successful_targets]]
namespace = "your-name"
repo = "your-repo"
branch = "main"
success_count = 1
last_success_at = 1783390000
```

最多保留 50 条。
最近成功的记录排在前面。

## 构建

```bash
cargo build --release
```

运行产物：

```bash
DRONE_NOTIFY_CONFIG=~/Scripts/config.toml ./target/release/drone_notify
```
