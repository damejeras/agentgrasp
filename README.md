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

agentgrasp runs each Bash command inside a `{ ... }` group that sends its output to files.
Claude Code asks for approval of every command that holds such a group, also when your allow
rules allow the command, and no allow rule changes that. So in Claude Code's default and
`acceptEdits` permission modes, every Bash call asks for approval. In `bypassPermissions` mode
the calls run without a prompt. Your deny rules still block a command in every mode.

When the Claude Code sandbox is on, let Bash write the agentgrasp state directory. Without
this, the output cannot go to its files, the shell prints the error, and the command does not
run:

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

## Links

- MCP Registry name: `mcp-name: io.github.damejeras/agentgrasp`
