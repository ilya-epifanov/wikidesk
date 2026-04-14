# wikidesk

A companion server for [LLM-wiki](https://github.com/karpathy/LLM-wiki) that turns any existing wiki repo into a shared knowledge service for multiple AI coding agents. wikidesk doesn't care how your wiki is organized, what agent runs the research (Claude Code, Pi, OpenCode, Codex, etc.), or what prompts you use -- it only requires a `wiki/` directory in the repo holding the actual wiki content. Set up your LLM-wiki however you like, then point wikidesk at it.

The primary workflow is simple: **agents read the `wiki/` directory directly** as a local knowledge base. On top of that, wikidesk optionally provides a `research` tool that lets agents request new research -- dispatching a dedicated research agent to investigate the question, update the wiki, and return an answer. Whether your agents can trigger research or only read is up to you, controlled by your agent rules.

## How it works

![wikidesk overview architecture](docs/diagrams/overview.drawio.svg)

1. Agents read the local `wiki/` directory for existing knowledge
2. When an agent needs new research, it submits a question (via MCP `research` tool or `POST /api/research`)
3. The server queues the question and spawns a research agent (configurable command)
4. The research agent investigates the question, potentially creating or updating wiki pages
5. The answer is returned with `[[wikilinks]]` resolved to file paths
6. Agents sync their local wiki copy automatically

## Agent rules

Configure your agents to use the wiki by adding rules to your `CLAUDE.md`, `AGENTS.md`, or equivalent:

<details>
<summary>Read-only (agents consult the wiki but never trigger research)</summary>

```markdown
## Wiki

* The `wiki/` directory contains a knowledge base on <your topics>.
  Consult it before making decisions in these areas.
* Do not modify wiki files directly.
```

</details>

<details>
<summary>Read + research (agents can request new research via MCP)</summary>

N.B.: tool names use Claude Code conventions in the snippet below.

```markdown
## Wiki

* The `wiki/` directory contains a knowledge base on <your topics>.
  Consult it before making decisions in these areas.
* Do not modify wiki files directly.
* When the wiki doesn't cover a topic you need, use the `mcp__wikidesk__research` MCP
  tool to request investigation. Poll `mcp__wikidesk__get_result` until the task completes,
  then sync your local wiki copy.
```

</details>

## Automatic wiki sync (client-server mode)

In client-server mode, the local `wiki/` directory needs to stay in sync with the server. You can automate this using your agent harness's lifecycle hooks to run `wikidesk sync` at the start and end of each session.

<details>
<summary>Claude Code -- hooks in settings.json</summary>

Add to your project's `.claude/settings.json`:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "wikidesk sync"
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "wikidesk sync"
          }
        ]
      }
    ]
  }
}
```

Or ask Claude Code to set it up for you:

> Set up hooks in `.claude/settings.json` so that `wikidesk sync` runs on `PreToolUse` (all tools) and `Stop`. The environment variables `WIKIDESK_SERVER_URL` and `WIKIDESK_WIKI_PATH` are already set in the shell.

</details>

<details>
<summary>Cline / Roo Code -- custom instructions with task hooks</summary>

Cline and Roo Code support `custom_modes` with tool-use hooks. Add a sync step to your custom mode's `whenToUse` or use the built-in command execution to run `wikidesk sync` at task boundaries. Refer to your extension's documentation for the exact hook configuration.

</details>

<details>
<summary>Other harnesses -- general approach</summary>

Most agent harnesses support some form of lifecycle hooks or pre/post commands. The pattern is:

1. **Before the agent starts working**: run `wikidesk sync` to pull the latest wiki state
2. **After the agent finishes**: run `wikidesk sync` to pick up any changes from concurrent research

Check your harness documentation for:
- **aider**: `--run` flag or `.aider.conf.yml` commands
- **OpenCode**: lifecycle hooks in configuration
- **Cursor**: task/command configuration in settings

</details>

## Security

> **The research agent runs with full permissions.** The `agent_command` typically includes flags like `--dangerously-skip-permissions` (Claude Code) or equivalent settings that grant the child agent unrestricted system access. This is intentional — the research agent needs to read and write wiki files AND query random websites — but it means **the research agent or the server spawning it must run inside a sandbox**. Of course you can allow-list specific tools, websites etc. but in general case you want the LLM-wiki agent to freely browse the internet.



Recommended approaches:
- **Docker/Podman**: Mount only the wiki repo and config into the container.
- **bubblewrap (bwrap)**: Minimal Linux sandboxing with filesystem and network restrictions.

I'm using a custom Nix+bubblewrap-based sandboxing tool (not yet released) for development.

## Server setup

### 1. Install wikidesk-server

<details>
<summary>macOS / Linux (pre-built binary)</summary>

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/ilya-epifanov/wikidesk/releases/latest/download/wikidesk-server-installer.sh | sh
```

</details>

<details>
<summary>Windows (pre-built binary)</summary>

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/ilya-epifanov/wikidesk/releases/latest/download/wikidesk-server-installer.ps1 | iex"
```

</details>

<details>
<summary>From source (any platform with Rust 1.88+)</summary>

```sh
cargo install wikidesk-server
```

</details>

### 2. Set up your LLM-wiki

Follow the [LLM-wiki setup instructions](https://github.com/karpathy/LLM-wiki) to create and configure your wiki repo. wikidesk only requires that the repo contains a `wiki/` subdirectory -- everything else (prompts, CLAUDE.md workflows, topic structure) is up to you.

### 3. Create a configuration file

See [`config.example.toml`](config.example.toml) for all options.

```toml
# config.toml
wiki_repo = "./my-wiki"
prompt_template = "prompt.md"

# SECURITY: This command runs UNSANDBOXED by default.
# See the Security section -- always run the server in a container.
agent_command = ["claude", "-p", "$PROMPT", "--dangerously-skip-permissions"]
```

### 4. Start the server

```sh
wikidesk-server --config config.toml
```

### 5. Run as a daemon (optional)

To keep the server running across reboots:

<details>
<summary>Linux -- systemd (user service)</summary>

```sh
mkdir -p ~/.config/systemd/user

cat > ~/.config/systemd/user/wikidesk.service << 'EOF'
[Unit]
Description=wikidesk research server
After=network.target

[Service]
Type=simple
WorkingDirectory=%h/wikidesk
ExecStart=%h/.cargo/bin/wikidesk-server --config %h/wikidesk/config.toml
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload
systemctl --user enable --now wikidesk
journalctl --user -u wikidesk -f  # check logs
```

The service is enabled across reboots, but systemd stops user services when the user logs out. To prevent that: `loginctl enable-linger $USER`

</details>

<details>
<summary>macOS -- launchd</summary>

```sh
cat > ~/Library/LaunchAgents/com.wikidesk.server.plist << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.wikidesk.server</string>
  <key>ProgramArguments</key>
  <array>
    <string>$HOME/.cargo/bin/wikidesk-server</string>
    <string>--config</string>
    <string>$HOME/wikidesk/config.toml</string>
  </array>
  <key>WorkingDirectory</key>
  <string>$HOME/wikidesk</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>$HOME/Library/Logs/wikidesk.log</string>
  <key>StandardErrorPath</key>
  <string>$HOME/Library/Logs/wikidesk.log</string>
</dict>
</plist>
EOF

launchctl load ~/Library/LaunchAgents/com.wikidesk.server.plist
tail -f ~/Library/Logs/wikidesk.log  # check logs
```

</details>

<details>
<summary>Windows -- Task Scheduler</summary>

```powershell
$action = New-ScheduledTaskAction `
  -Execute "$env:USERPROFILE\.cargo\bin\wikidesk-server.exe" `
  -Argument "--config $env:USERPROFILE\wikidesk\config.toml" `
  -WorkingDirectory "$env:USERPROFILE\wikidesk"

$trigger = New-ScheduledTaskTrigger -AtLogOn

$settings = New-ScheduledTaskSettingsSet `
  -AllowStartIfOnBatteries `
  -DontStopIfGoingOnBatteries `
  -RestartCount 3 `
  -RestartInterval (New-TimeSpan -Seconds 10)

Register-ScheduledTask `
  -TaskName "wikidesk" `
  -Action $action `
  -Trigger $trigger `
  -Settings $settings `
  -Description "wikidesk research server"
```

</details>

<details>
<summary>Docker</summary>

```sh
docker run -d \
  --name wikidesk \
  --restart unless-stopped \
  -v /path/to/wiki-repo:/wiki \
  -v /path/to/config.toml:/etc/wikidesk/config.toml:ro \
  -p 1238:1238 \
  wikidesk-server --config /etc/wikidesk/config.toml
```

This also provides sandboxing for the research agent.

</details>

## Consumer workspace setup

There are two ways for agents to consume the wiki. Choose one.

### Client-server mode (recommended)

Each agent machine runs `wikidesk`, which communicates with the server over HTTP. The client syncs a local wiki copy automatically after each research request.

![Client-server deployment mode](docs/diagrams/client-server.drawio.svg)

#### Install wikidesk

<details>
<summary>macOS / Linux (pre-built binary)</summary>

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/ilya-epifanov/wikidesk/releases/latest/download/wikidesk-installer.sh | sh
```

</details>

<details>
<summary>Windows (pre-built binary)</summary>

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/ilya-epifanov/wikidesk/releases/latest/download/wikidesk-installer.ps1 | iex"
```

</details>

<details>
<summary>From source (any platform with Rust 1.88+)</summary>

```sh
cargo install wikidesk
```

</details>

#### Configure and use

```sh
export WIKIDESK_SERVER_URL="http://your-server:1238"
export WIKIDESK_WIKI_PATH="./local-wiki"

# Submit a question and sync wiki
wikidesk research "What is RLHF and how does it relate to DPO?"

# Sync wiki only
wikidesk sync
```

### Mount/symlink mode

Agents connect to the server directly via MCP. The wiki directory is mounted or symlinked into each agent's workspace for read access.

![MCP-only deployment mode](docs/diagrams/mcp-only.drawio.svg)

> **Caution:** Ensure agents have **read-only** access to the wiki. Writing directly bypasses the server's research workflow and causes conflicts with concurrent research agents.

#### Configure MCP

Add wikidesk to your agent's MCP configuration:

```sh
claude mcp add wikidesk --transport http http://your-server:1238/mcp
```

Or add it manually to `.mcp.json`:

```json
{
  "mcpServers": {
    "wikidesk": {
      "type": "streamable-http",
      "url": "http://your-server:1238/mcp"
    }
  }
}
```

The server exposes two MCP tools:

- **`research`** -- Submit a research question. Returns a `task_id`.
- **`get_result`** -- Poll for the result of a research task.

#### Mount the wiki (read-only)

<details>
<summary>Linux / macOS -- symlink</summary>

```sh
ln -s /path/to/wiki-repo/wiki ./wiki
```

Simplest option when the server and agent share a filesystem.

</details>

<details>
<summary>Linux / macOS -- NFS or network mount</summary>

```sh
# Export on the server (add to /etc/exports):
#   /path/to/wiki-repo/wiki  agent-host(ro,no_subtree_check)

# Mount on the agent machine:
sudo mount -t nfs -o ro server-host:/path/to/wiki-repo/wiki ./wiki
```

</details>

<details>
<summary>Docker</summary>

```sh
docker run ... -v /path/to/wiki-repo/wiki:/workspace/wiki:ro ...
```

The `:ro` flag ensures the container cannot write to the wiki.

</details>

<details>
<summary>Podman</summary>

```sh
podman run ... -v /path/to/wiki-repo/wiki:/workspace/wiki:ro,Z ...
```

The `Z` option handles SELinux relabeling.

</details>

<details>
<summary>Windows -- symbolic link</summary>

```powershell
# Requires Developer Mode or elevated prompt
New-Item -ItemType SymbolicLink -Path .\wiki -Target C:\path\to\wiki-repo\wiki
```

</details>

<details>
<summary>Windows -- WSL2</summary>

```sh
# From within WSL2, the Windows filesystem is at /mnt/c/
ln -s /mnt/c/path/to/wiki-repo/wiki ./wiki
```

</details>

## Configuration reference

| Key | Default | Description |
|-----|---------|-------------|
| `wiki_repo` | `.` | Path to the wiki git repo (must contain `wiki/` subdirectory) |
| `bind_address` | `127.0.0.1:1238` | HTTP bind address |
| `agent_command` | *(required)* | Command to spawn the research agent. Must contain exactly one `$PROMPT` element. |
| `prompt_template` | *(required)* | Path to prompt template file (must contain `{question}` placeholder) |
| `instructions` | *(optional)* | Instructions shown to MCP clients |
| `research_tool_description` | *(optional)* | Custom description for the `research` MCP tool |
| `completed_task_ttl_secs` | `900` | How long to keep completed task results (seconds) |
| `agent_timeout_secs` | `1800` | Maximum time an agent may run before being killed (seconds) |

## TODO

- [ ] Add simple UI for monitoring research request queues
- [ ] Manage multiple wikis, expose at different base HTTP contexts
- [ ] Add an optional simple fixed git workflow: `git add .` → ask agent to commit in a loop until fixed point → `git push` (optional)
- [ ] Support Claude's streaming-json output mode, ACP for better progress monitoring

## See also

- **[llmwiki-tool](https://github.com/ilya-epifanov/llmwiki-tooling)** -- a companion CLI for wiki maintenance: fixing broken links, renaming pages with reference updates, detecting orphans, and linting against configurable rules

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
