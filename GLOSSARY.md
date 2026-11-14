# QQBot

Messages, sessions, participation modes and user operations between the QQ bot and the conversation agent.

## Language

**Bridge layer (桥接层)**:
The layer between QQ messaging and the conversation agent that connects messages, sessions, tool operations and the bot's participation behaviour.

**Group session (群会话)**:
A session whose counterpart is one QQ group; it can contain messages from many group members.

**Private session (私聊会话)**:
A one-to-one session with a single QQ user.

**Normal chat (普通聊天, `chat`)**:
The participation mode that provides ordinary chat in allowed group and private sessions.

**Admin agent (管理员代理, `closed-agent`)**:
The participation mode that opens the full agent toolset, including questions and approvals, in the administrator's private session.

**First-generation simulation (一代仿真, `reserved`)**:
The first-generation participation mode that imitates a group member through participation states such as watching, active, probing and leaving.

**Second-generation simulation (二代仿真, `reserved2`)**:
The second-generation participation mode in which the agent itself reads messages, speaks, waits, and sets wake-ups or lurks.

**Wake (唤醒)**:
One occasion on which the bridge hands a conversation to the agent: a message, a timer or a check that calls for its attention.
_Avoid_: trigger, tick

**Briefing (简报)**:
The stable part of what the agent is told about a conversation: its role, behaviour and sending rules, stickers, memory items and participation context. Given in full once per session generation, then only where it changed.
_Avoid_: notes, full prompt, initial prompt

**Wake delta (唤醒增量)**:
What one wake tells the agent beyond the briefing: why it woke, the new messages, the current state, and any changed parts of the briefing.
_Avoid_: wake prompt, diff

**Session generation (会话代次)**:
The span of one agent session's history between two points where it was started, rotated or compacted; each generation begins with a full briefing.

**Session rotation (会话轮换)**:
Replacing a conversation's agent session with a fresh one while the bot sleeps, after saving what matters to memory, once the old session has grown past its cap; the old session is archived, never deleted.
_Avoid_: reset, idle compaction, autocompact

**Cache life (缓存寿命)**:
How long a model provider keeps a session's prompt cached between calls; a call after a longer gap pays full price for the whole context.

## Operations

**Supervisor (监督进程)**:
The single long-lived Rust process, started by the one scheduled task, that owns the bridge's processes: it starts, adopts, restarts and updates them.
_Avoid_: watchdog (a task the supervisor runs), overwatch

**Dependency (依赖进程)**:
An external process the bridge needs, namely SnowLuma (the OneBot gateway), the model relay (CLIProxyAPI) and DSH (the conversation agent host), that the supervisor starts, adopts and restarts and that outlives the supervisor.
_Avoid_: resident, service

**Release (发布版本)**:
One versioned build of the bridge, installed side by side with others in its own folder; the supervisor runs exactly one release at a time.
_Avoid_: install, package, candidate

**Current release (当前版本)**:
The release the scheduled task starts and the supervisor runs; exactly one at a time.

**Last-good release (上一可用版本)**:
The most recent release that passed the health check after becoming current; the rollback target.

**Trial (试运行)**:
A new release running beside the current one, before it is made current, so its health can be judged.

**Update policy (更新策略)**:
The administrator's setting for bridge releases: off, notify (ask before installing) or auto. Dependencies always need approval.

**Health check (健康检查)**:
The single definition of "running": desired state running, supervisor alive, and console, SnowLuma, the configured model relay and DSH all healthy, including a working DSH login. People, scripts and the updater share it.

**Pinned version (锁定版本)**:
The exact version of a dependency the supervisor runs; it changes only when the administrator approves an upgrade.

**Install root (安装根目录)**:
The single folder holding everything production runs: releases, dependencies, the runtime and backups. Development never uses it.
_Avoid_: production folder, deploy dir

**Harness (运行框架, `harness`)**:
HER's own part that runs a conversation turn: it sends the prompt to a model, runs the tool calls the model asks for, and keeps the conversation's history and summaries. It takes over the role DSH plays today.
_Avoid_: DSH, conversation engine, agent host, backend

**Upstream (上游)**:
An outside project HER takes code or ideas from (such as DSH, SnowLuma, AstrBot or llama.cpp); HER keeps only what it needs and watches the project's releases for improvements to adopt.
_Avoid_: dependency (a process the supervisor runs), fork, vendor

## Companion

**Rig (算力主机)**:
The owner's GPU gaming PC, lent to HER for local compute; the owner's own use always comes first.
_Avoid_: node, server, worker

**Local compute (本地算力)**:
Model work done on the owner's own machines instead of a cloud API.
_Avoid_: on-prem, self-hosted

**Game mode (游戏模式)**:
The Rig state in which HER gives the GPUs back to the owner and falls back to cloud compute.

**Emotion state (情绪状态)**:
HER's live, scored mood, moved by everything she says, sees, hears and remembers, and shown in her text, voice and avatar.
_Avoid_: mood label, sentiment

**Hand (手)**:
A capability through which HER acts outside chat, such as a phone or a code workspace; each one is switched and limited from the console.
_Avoid_: tool, plugin, skill

**Eval set (评测集)**:
Real QQ conversations, de-identified and kept only on the owner's machines, used to judge whether a change makes HER more human-like.
_Avoid_: dataset, benchmark

**Eval site (评测站)**:
The public, login-free website where anyone with the link blind-compares two candidate replies; it holds only public-safe items, and real group chats are judged only in the console on the owner's machines.
_Avoid_: eval website, voting page

**Public-safe item (可公开条目)**:
An evaluation item that may appear on the public eval site: a synthetic conversation, or an item from the owner's private chat that the owner explicitly marked public.
_Avoid_: anonymised chat, sample

**Skill (技能)**:
A saved how-to note that HER loads only when a task needs it, such as where to look up the weather; HER may draft or improve one, and it takes effect only after the owner approves it.
_Avoid_: hand, plugin, tool, memory

**Plugin (插件)**:
An add-on from outside HER, such as a community package, that brings new abilities and may bring skills; only the owner installs one.
_Avoid_: hand, skill, extension

**Memory layers (记忆分层)**:
The four parts of what HER is given about the past, each with its own budget: **Identity** (her persona and red lines, always present), **Relationship** (impression cards and shared history with the people in the conversation, always present), **Conversation** (this chat's ongoing topics, norms and pending thoughts) and **Material** (stored records retrieved only when relevant).
_Avoid_: memory tiers, context blocks

**Persona (人设)**:
An owner-editable, versioned description of who HER is and how she talks: a **persona core** (人设核心) present in every request (the Identity layer), **persona notes** (人设资料) recalled only when relevant, and an owner-approved **compact persona** (精简人设) used when the core cannot fit. Edits arrive automatically within the owner's rules (auto) or only on approval (manual).
_Avoid_: character bible, role card, preset, system prompt

**Trait (性格参数)**:
A fixed per-persona setting that shapes behaviour without changing moment to moment, such as **neediness** (粘人度, 0-3: how readily she reaches out first and says she misses someone); unlike the Emotion state, a trait changes only when the persona is edited.
_Avoid_: mood, emotion, score

**Mode prompt (仿真提示词)**:
The behaviour and tool rules of one participation mode (how to read, wait, reply and stay silent), separate from any persona.
_Avoid_: persona, preset, system prompt

**Rule set (规则集)**:
HER's global rules, kept outside every persona and each with its own on/off switch: **red lines** (红线), which the bridge enforces itself without telling the model, and **triggered rules** (触发规则), which add a one-turn instruction only when their signal appears (a sincere "are you an AI?", a crisis).
_Avoid_: guardrails, safety prompt, policy

**Boundary switch (边界开关)**:
One of four rule-set entries about how close and honest she is: admit being an AI and the distress protocol (both triggered rules), push back and encourage other relationships (each one short line in the persona core while on).
_Avoid_: safety setting, guardrail

**Schedule (日程)**:
The console's single list of everything that will happen at a set time, whoever set it up: system jobs, the owner's jobs and reminders, and HER's own wake timers, planned reach-outs and reminders.
_Avoid_: tasks page, cron, timers

**Impression card (印象卡)**:
HER's current, versioned picture of one person in one scope, rebuilt from linked evidence and visible only where all of that evidence is visible.
_Avoid_: profile, persona, user summary

**Fact (事实) / Inference (推断)**:
A memory's provenance: a fact is something a person actually said or did, linked to its message; an inference is anything a model wrote, including impression cards and digests, and never becomes a fact.
_Avoid_: truth, belief, derived memory

**Digest (记忆整理)**:
The run that reads a conversation's stored messages since its cursor, asks a model what is worth remembering and stores the answer as inferences whose provenance is the conversation and message range; the nightly job, the backlog check and idle rotation all use it.

**Checking digest (核对整理)**:
The digest pass that first re-checks card lines and inferences against the messages they cite and supersedes those no longer supported, then rebuilds cards within their size cap; it runs where conversations changed.
_Avoid_: dreaming, consolidation job, memory cleanup

**Session inspector (会话检视)**:
The console view that shows what happens inside each conversation as it happens: every turn, the model's reasoning, tool calls and their results, tokens and cost.
_Avoid_: log viewer, debug page
