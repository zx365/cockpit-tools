# Cockpit Tools 私有定制版

本项目基于 [jlcodes99/cockpit-tools](https://github.com/jlcodes99/cockpit-tools) 开发，并持续合并原版更新。

当前私仓：[zx365/cockpit-tools](https://github.com/zx365/cockpit-tools)

## 原版能力

保留 Cockpit Tools 的多平台 AI IDE 账号管理、账号切换、多实例运行、额度查询、自动唤醒、API 服务及各平台账号管理能力。

## 本版调整

- 增加 Codex 今日用量、每日用量区间与成本统计。

## OpenClaw 微信功能

- 支持从 Cockpit Tools 一键初始化 OpenClaw Gateway 并绑定微信。
- 自动复用 Cockpit Tools 当前 Codex 登录信息，无需在 OpenClaw 中重复登录。
- 可设置 OpenClaw 使用的模型和思考强度，默认使用 `openai/gpt-5.6-luna` 与 `low`。
- 绑定微信时自动读取当前进程的 `HTTP_PROXY`、`HTTPS_PROXY`，写入 OpenClaw Gateway 环境并启用 `env-proxy`。
- 支持发送微信测试通知。
- Codex 的 5 小时额度或周额度进入新周期后，可通过微信发送恢复通知。
- 额度通知包含账号、Team Name 和恢复后的额度，便于区分同一邮箱下的多个团队。

通知示例：

```text
Cockpit Tools：Codex 额度已恢复
账号：example@example.com
Team Name：Example Team
5 小时额度：100%
```

## 上游同步

原版仓库：<https://github.com/jlcodes99/cockpit-tools>

本项目仅维护上述定制差异，其余功能与修复优先跟随原版。
