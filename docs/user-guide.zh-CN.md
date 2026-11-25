# HER 中文使用指南

本指南供在 Windows 上管理 HER 的管理员使用。HER 通过 SnowLuma 接入 QQ，由 Rust 监督进程管理桥接、控制台和已配置的依赖。本文以安装版的 `current` 和 `runtime` 目录为例。实际安装路径、主机上的运维脚本和故障处理步骤见私有仓库 `H.E.R.-vault` 中的 `docs/agents/production-ops.md`。请只在已确认的安装上执行操作。 [生产操作约定](agents/production-ops.md)

## 启动、停止和重启

1. **打开控制台**。在安装主机上打开 `current\her.exe`，或点击 Windows 托盘里的 HER 图标。关闭窗口会隐藏到托盘，退出托盘会结束桌面程序，监督进程继续运行。Windows 登录时，已注册的 `HER` 计划任务负责启动监督进程。 [Windows 启动约定](agents/windows-lifecycle.md)
2. **手动启动**。监督进程未运行时，以安装账户运行已注册的 `HER` 任务，也可用下表中的 PowerShell 命令隐藏启动。如果之前主动停止过，先恢复运行状态，启动后再检查健康。
3. **停止**。停止命令把期望状态设为停止，并停止桥接与控制台，依赖会继续运行。需要连同已配置的依赖一起停机时，加 `--with-dependencies`。如需临时保留控制台以便检修，可在计划页选择维护状态。
4. **重启控制台**。在界面确认重启，然后到计划页（`/schedule`）的运行历史查看结果。

使用 PowerShell 时，先把示例路径改成实际的绝对路径，再从下表选择所需命令。

```powershell
$Install = 'D:\HER'
$Runtime = Join-Path $Install 'runtime'
$Her = Join-Path $Install 'current\her.exe'
$HerCli = Join-Path $Install 'current\her-cli.exe'
```

| 操作 | PowerShell 命令 |
| --- | --- |
| 启动监督进程 | `Start-Process -FilePath $Her -ArgumentList ('supervise --runtime-root "{0}" --install-root "{1}"' -f $Runtime, $Install) -WindowStyle Hidden` |
| 恢复运行状态 | `& $HerCli lifecycle set running --runtime-root $Runtime --install-root $Install` |
| 停止桥接与控制台 | `& $HerCli stop --runtime-root $Runtime --install-root $Install --confirm` |
| 检查健康 | `& $HerCli lifecycle assert-running --runtime-root $Runtime --install-root $Install --wait 90s` |

健康检查返回 `ok: true` 时，各项检查均已通过。如果失败，先读 `parts` 中失败项的原因。托盘显示需要登录 QQ 时，在安装主机上打开对应的 QQ 窗口完成登录，再检查健康。 [生命周期与健康检查](agents/lifecycle-tasks.md)

## 更新 HER 和维护依赖

- **HER 更新**。在控制台查看可用版本和更新策略。`notify`（默认）在有更新时通知，由管理员确认界面所列版本。`auto` 会等到空闲再更新。`off` 关闭版本检查。监督进程验证签名与校验和，把新版本放在独立目录试运行。试运行未通过健康检查会自动回退。更新后核对当前版本和健康状态。 [发布与试运行](agents/lifecycle-tasks.md)
- **SnowLuma 与 DSH 升级**。在依赖区刷新，核对 SnowLuma 及已启用的 DSH 的锁定版本和可用版本，再确认要升级的那一项。即使 HER 设为自动更新，依赖也要逐项批准。升级后检查版本与健康状态，并查看是否发生回退。 [依赖升级约定](agents/lifecycle-tasks.md)
- **模型中继（如果已配置）**。按私有运维手册安装并维护锁定版本。变更后查看健康检查中的 `relay` 项。
- **依赖异常**。先看健康检查和管理员私聊通知。监督进程会按退避间隔恢复已确认归属的依赖。如果提示端口被占用或无法核实归属，先按私有手册核实再继续操作。

### 安全重启 DSH

只有仍运行 DSH 的安装才需要这一步。安排好维护时段，先等正在处理的会话结束，再在安装主机用该机的 `restart-dsh.ps1`，按私有运维手册指定运行目录和脚本要求的路径，并确认操作。脚本按配置端口及实际 Node 入口核实进程归属。遇到归属不明或脚本拒绝时，保留现场并按手册核对。重启完成后确认健康检查中的 DSH 和 DSH 登录项恢复。仅重启 DSH 时保留现有会话。 [DSH 重启约定](agents/windows-lifecycle.md)

## 人设与角色卡

- 在控制台打开人设页（`/personas`），填写名称、核心设定、资料和精简设定，再保存版本。设为默认人设，或为指定群聊或私聊设置覆盖。手动审批模式下，在版本列表批准拟启用的版本，再核对活动版本。
- 已有角色卡可通过人设页的 **Import role cards** 导入。导入后核对内容，再分配给会话。伴聊会话使用人设。管理员助手（closed-agent）有独立的角色卡设置。
- 名字字段也可用作伴聊的唤醒词。给指定会话换人设后，可在人设页核对该会话的唤醒词。 [人设与角色卡约定](agents/prompts-modes.md)

## 语音

- **发送录音**。把准备好的短录音放进运行目录下配置的语音库，在控制台选目标会话及录音，先预检时长和大小，再发送并检查结果。MP3、WAV、OGG 等格式需要本机的 `ffprobe` 核实时长，音量处理使用 `ffmpeg`。QQ 语音时长上限为 60 秒，实际还受本机配置的更短上限及频率限制。
- **让 HER 自己发语音**。语音和发送语音工具默认关闭。需要时在设置中同时启用 `socialV2.voice.enabled` 和 `sendVoice`，并检查语音库目录及发送限额。HER 从库中选现成录音。
- **接收语音转文字**。仅在已配置本机转写助手、且列入 `voiceTranscription.conversations` 的 reserved2 会话启用。默认保留 `[语音]` 标记，转写失败时也会保留该标记。
- **发送结果不明**。先在 QQ 内核对原消息及控制台发件箱，再决定是否手动补发。 [语音与投递约定](agents/media-transport.md)

## 用手机管理

在手机 QQ 上用管理员账户私聊 HER，接收依赖、登录、会话和更新通知。整条消息只发对应命令。

| 命令 | 用途 |
| --- | --- |
| `/persona` | 查看默认和可用人设 |
| `/persona <人设名>` | 切换默认人设 |
| `/persona <人设名> group:<群号>` | 为指定群聊设置人设。私聊用 `private:<QQ号>` |
| `/persona off group:<群号>` | 清除该群的人设覆盖，恢复使用默认人设 |
| `/update` | 批准已发现的 HER 更新 |
| `/update deps` | 批准列表中首项可用的依赖升级 |

两个更新命令在有候选版本时会批准执行。只想查看版本时，请用控制台。收到回执后，到控制台核对最终结果。控制台仅在安装主机的回环地址监听，涉及登录、启停、备份和其它维护时，请回到安装主机操作。

## 日常巡检

1. **先看运行状态**。查看托盘、控制台以及管理员私聊。如果 HER 收得到消息却不回复，在控制台核对会话是否已接入、是否命中拒绝列表、参与模式和暂停状态。会话接入范围变更后，等待监督进程应用新的范围。
2. **处理暂停和缺口**。先在控制台核对暂停回合、入站缺口和发件箱的状态。若管理员私聊通知带有恢复码，按通知选择放弃或重试。结果不明时先确认 QQ 内的实际投递情况。
3. **确认计划和备份**。在计划页查看最近的记忆整理、备份和维护任务结果。需要恢复时，在控制台先预览备份的替换范围，确认后再执行，并检查健康状态。 [会话恢复约定](agents/sessions-recovery.md) · [计划与备份约定](agents/lifecycle-tasks.md)
