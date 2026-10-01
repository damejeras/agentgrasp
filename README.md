# agentgrasp

agentgrasp keeps long command output out of a Claude Code agent's context. Claude Code still
runs every Bash command. When the output is long, agentgrasp saves it to a file and the agent
sees a short summary with the file path. The agent can then read the file, or ask yes/no
questions about it with the `ask` tool. The `search` tool finds the files and line ranges that
are relevant to a question, without returning source text.

`ask` and `search` use Jev, TypeSafe's yes/no model.

## Install

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/damejeras/agentgrasp/releases/latest/download/agentgrasp-installer.sh | sh
claude plugin marketplace add damejeras/agentgrasp
claude plugin install agentgrasp@agentgrasp
```

The plugin calls `agentgrasp` from `PATH`. Set `TYPESAFE_API_KEY` in the environment that
starts Claude Code; without it, `ask` and `search` return `provider_unavailable`.

## Settings

agentgrasp adds two lines around each Bash command. A plugin cannot add permission rules, so
add these allow rules to your user settings (`~/.claude/settings.json`). Without them, every
Bash call asks for approval. Use the path that `command -v agentgrasp` prints:

```json
{
  "permissions": {
    "allow": [
      "Bash(unset TYPESAFE_API_KEY)",
      "Bash(/home/you/.cargo/bin/agentgrasp finish:*)"
    ]
  }
}
```

The rules allow only these two lines. Every other part of a command still needs your own
rules, and your deny rules still block it.

When the Claude Code sandbox is on, let Bash write the agentgrasp state directory, so the exit
status and duration of each command are recorded:

```json
{
  "sandbox": {
    "filesystem": {
      "allowWrite": ["~/.local/state/agentgrasp"]
    }
  }
}
```

Use `$XDG_STATE_HOME/agentgrasp` instead when `XDG_STATE_HOME` is set.

## Costs and risks

- `ask` and `search` send file content to TypeSafe. TypeSafe lists Jev at $42 per billion
  input tokens. Each call writes the tokens it used to its record under
  `~/.local/state/agentgrasp/asks/` or `searches/`.
- `ask` and `search` can read any file inside the MCP roots, which are the project directory
  and the directories you add. This includes files that your `Read` deny rules block. When you
  allow these tools, you allow that.
- The state directory grows until you delete files from it. agentgrasp never deletes them.
- A runaway command can fill the disk until Claude Code's timeout stops it.
- Claude Code gives agentgrasp stdout and stderr as one stream, so the saved output has them
  together, in the order Claude Code captured them.
- A Bash call always ends with status 0, so Claude Code reports a failed command as a success;
  the summary gives the command's real exit code.
- A command that ends the shell with a non-zero status, such as `exit 1` or `exec false`, skips
  the summary. Claude Code then shows its own, shortened output.

## Links

- MCP Registry name: `mcp-name: io.github.damejeras/agentgrasp`
