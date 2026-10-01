# agentgrasp V1 specification

agentgrasp keeps long command output out of a coding agent's context. Claude Code still runs
every Bash command, but the output goes to files. The agent sees the output only when it is
short. For long output, the agent sees the file paths. It then asks yes/no questions about
the files, or reads them with its own tools. agentgrasp also finds the files that are relevant
to a natural-language question, without returning source text.

This file says what to build. Delete it when V1 is built. The tests, the schemas and the tool
descriptions are then the record.

## Parts

One Rust binary, `agentgrasp`, with three subcommands:

| Subcommand | Job |
| --- | --- |
| `hook` | Claude Code PreToolUse hook. It rewrites each Bash command so that its output goes to files. |
| `finish` | The last step of a rewritten command. It records the result and prints what the agent sees. |
| `mcp` | Local stdio MCP server with two tools: `ask` and `search`. |

Use Rust edition 2024, `rmcp` (features `server`, `transport-io`, `macros`), `tokio`, `serde`,
`serde_json` and `libc`, as `../agentchan` does. Use the latest release of each crate.
Use `reqwest` for the Jev API, `ignore` and `globset` for file walking, `sha2` for hashes and
`tree-sitter` with the grammar crates for Python, Go, Rust, TypeScript, TSX and JavaScript.

Only two values come from outside the binary: the state directory, from `$XDG_STATE_HOME`
(`~/.local/state` when it is not set), and the Jev key, from `TYPESAFE_API_KEY`. Every other
value in this file is a constant in the code. There is no config file.

Claude Code is the only supported agent.

## Hook

`agentgrasp hook` reads the PreToolUse input JSON on stdin.

1. If `tool_name` is not `Bash`, or `tool_input.run_in_background` is true, exit 0 with no
   output. The call runs unchanged.
2. Find the shell that Claude Code runs: `CLAUDE_CODE_SHELL` when it names an executable bash
   or zsh, else `$SHELL`. If that is not bash or zsh, pass the call through unchanged. When
   the shell is not certain, pass through.
3. Create a capture directory `D = <state>/agentgrasp/captures/<id>`. `<id>` is unique and
   sorts by time.
4. Write `D/metadata.json` with the original command, the working directory and the start time.
5. Build the rewritten command (below). Check its syntax with the same shell, with no startup
   files and a 2 s timeout: `bash --norc --noprofile -n` or `zsh -f -n`. If the check fails or
   times out, pass the call through unchanged.
6. Print `hookSpecificOutput` with `hookEventName: "PreToolUse"` and `updatedInput`. Copy every
   field of `tool_input` and replace only `command`. Do not set `permissionDecision`. Claude
   Code then applies the user's permission rules to the rewritten command as usual.

If any step fails, write the reason to stderr and exit 0 with no output. The command then
runs unchanged.

The rewritten command keeps the original command as visible text. Claude Code checks each
part of a compound command against the permission rules, so a deny rule on the original
command still blocks it. Never encode, quote or escape the original command.

```sh
echo 'agentgrasp: D'; { unset TYPESAFE_API_KEY
<original command>

} > 'D/stdout.log' 2> 'D/stderr.log' < /dev/null; '/abs/path/agentgrasp' finish 'D' $?
```

- `D` and the binary path are absolute. Quote them in single quotes and escape any single
  quote in them. Use the absolute path of the running `agentgrasp` binary, so a `PATH` change
  or a shell function in the command cannot replace `finish`.
- The original command goes on its own lines. The blank line after it stops a trailing
  backslash from joining the closing `}`.
- The brace group runs in Claude Code's shell, so a `cd` carries over to the next call as it
  does without the hook.
- `exit`, `exec` or `set -e` in the command can end the shell before `finish` runs. The first
  `echo` has already printed the capture directory, so the agent still knows where the logs
  are. The same holds when Claude Code kills the command at its timeout.
- The logs have no size limit. A runaway command can fill the disk until Claude Code's timeout
  stops it.

Claude Code runs the rewritten command with its own timeout and process handling. agentgrasp
does not start, time or kill the command.

The parts that the wrapper adds (`echo`, `unset`, `agentgrasp finish`) must not cause a
permission prompt. A command that the user's rules allow must stay allowed after the
rewrite. When a part needs an allow rule, the plugin or the README supplies it.

When the Claude Code sandbox is on, Bash can write the logs only if
`sandbox.filesystem.allowWrite` includes the state directory. The README gives that setting.
Without it, the redirection fails, the shell prints the error and the command does not run.
agentgrasp never runs a command again on its own.

## Finish

`agentgrasp finish D CODE` runs after the brace group ends. `CODE` is the command's exit
status.

1. Read the sizes of `D/stdout.log` and `D/stderr.log`.
2. Add the exit status, the end time, the duration and the sizes to `D/metadata.json`.
3. Print what the agent sees (below).
4. Exit with `CODE`, also when a step above fails. Claude Code then reports a failed command
   as failed.

When stdout and stderr together are 2048 bytes or less, print stdout. If stderr is not empty,
print a line `--- stderr` and then stderr. Then print the summary. Read at most the sizes
found in step 1, so a log that still grows cannot make `finish` read without limit.

When they are more than 2048 bytes, print only the summary:

```text
exit_code: 1
stdout: 120 bytes D/stdout.log
stderr: 48000 bytes D/stderr.log
duration_seconds: 6.4
output not shown; ask yes/no questions with the agentgrasp ask tool, or read the files
```

The order of lines between stdout and stderr is not known, so never print them merged.

A process that the command left running in the background can still write to the logs after
`finish`. The sizes in `metadata.json` describe the files at the time `finish` ran. `ask`
hashes the bytes it actually reads.

## MCP server

`agentgrasp mcp` is a local stdio MCP server. Protocol traffic uses stdout. Diagnostics use
stderr. Neither one ever contains command output or source text.

### Common rules

- Each tool has an input schema and an output schema. The output schema is one object, and
  it is the same for success and for failure.
- The response repeats each question exactly and gives `p_yes`, a number from 0 to 1.
  Questions are plain strings in an ordered array. The server gives them positional keys for
  Jev. Duplicate questions keep their own positions.
- Answers are independent. Several can be high, and they need not sum to one.
- Every response has `model`: the model that Jev reports it used, or null when no request was
  made.
- `error` is null on success. On failure it is `{code, message}`. The message never contains
  command output, source text, credentials or a provider response body.
- Error codes: `invalid_input`, `source_unreadable`, `source_changed`, `unsupported_encoding`,
  `input_too_large`, `provider_unavailable`, `invalid_provider_response`,
  `budget_exhausted`, `cancelled`.
- An invalid argument (`invalid_input`) sets MCP `isError: true`. No file is read and no
  request is made. All other failures leave `isError` unset; the client reads `error`.
- A missing `TYPESAFE_API_KEY` gives `provider_unavailable`.
- Allowed locations: after initialization, the server asks the client for its MCP roots
  (`roots/list`) and follows root change notifications. A path is allowed when its
  canonical form is inside a root, or inside a capture directory whose recorded working
  directory is inside a root. Anything else gives `invalid_input`. Check the supplied path
  and the resolved path. Root containment does not apply the user's `Read` deny rules.
- Malformed MCP protocol requests follow the MCP protocol error contract.

### `ask`

`ask` answers yes/no questions about a set of local files. It never runs a command.

Input:

```json
{
  "paths": ["/home/me/.local/state/agentgrasp/captures/<id>/stderr.log"],
  "questions": [
    "Does the diagnostic identify a missing import?",
    "Does the diagnostic identify a database CHECK constraint violation?"
  ]
}
```

- `paths`: a nonempty array of absolute paths in allowed locations. A path that matches the
  credential-file exclusion of `search`, before or after resolving, gives `invalid_input`. A
  missing, unreadable or non-regular file gives `source_unreadable`.
- `questions`: 1 to 64 nonempty strings, each at most 2048 bytes.

Response:

```json
{
  "answers": [
    {"question": "Does the diagnostic identify a missing import?", "p_yes": 0.02},
    {"question": "Does the diagnostic identify a database CHECK constraint violation?", "p_yes": 0.97}
  ],
  "evaluation_status": "complete",
  "model": "jev-1.13.0",
  "sources": [
    {"path": "/home/me/.local/state/agentgrasp/captures/<id>/stderr.log", "bytes": 48000, "sha256": "…"}
  ],
  "record_path": "/home/me/.local/state/agentgrasp/asks/<id>/record.json",
  "error": null
}
```

- `evaluation_status` is `complete` or `failed`. A complete response has exactly one answer
  per question, in input order. A failed response has `answers: []` and an error. A partly
  valid answer set from Jev is a failure. A missing probability is never replaced with zero.
- Each question is answered over all the files together. Each file goes into the Jev state
  with its path, so the boundaries between files stay clear.
- Paths that resolve to the same file are read once. The first path given is the one shown.
  A file symlink to a regular file is accepted.
- Read each file once into memory and hash those bytes. If the file changed during the read,
  read it again, up to 3 times, then fail with `source_changed`.
- The files together may hold at most 256 KiB. Enforce this while reading the opened files,
  not only from their sizes. Above it, stop and return `input_too_large` without a request.
  This is a resource limit. It does not prove the request fits Jev.
- Every file is sent in full or the request fails. A file that is not valid UTF-8 gives
  `unsupported_encoding`. Nothing is clipped, skipped or replaced.
- Jev's token limit cannot be checked locally. Send the request. When TypeSafe rejects it for
  size, return `input_too_large`.
- One `ask` call, with its retries and backoff, ends within 60 s. Past that, return
  `provider_unavailable`.
- `sources` lists only the files read before a failure.
- Write `record.json` with the questions, the answers, the model, the usage, the latency and
  the sources. It never contains file content.

The tool description must say:

- `ask` answers yes/no classification questions only, as a probability of yes. It cannot
  quote, extract, count, summarise or explain. For those, read the file.
- Treat `p_yes` ≥ 0.9 as yes and ≤ 0.1 as no. Between those values, read the file.
- A low `p_yes` means a probable no. It does not prove that the files hold no relevant text.
- On `input_too_large`, use grep or read a part of the file.
- `ask` reads the files given and writes a record under the state directory.

### `search`

`search` finds files and source regions likely to help investigate a natural-language
question. It returns ranked locations and relevance scores. It never returns source text or
a generated answer.

Port the search of [dzhng/jevgrep](https://github.com/dzhng/jevgrep) at commit
`82ef1fd3f43161bb17395dba1e07a5338f3913db` to Rust. Keep its prompts
([`packages/core/src/requests.ts`](https://github.com/dzhng/jevgrep/blob/82ef1fd3f43161bb17395dba1e07a5338f3913db/packages/core/src/requests.ts)),
its thresholds and its budgets as they are. Keep its MIT license notice with the ported code.
Do not port its cache. Its design is described in its
[architecture](https://github.com/dzhng/jevgrep/blob/82ef1fd3f43161bb17395dba1e07a5338f3913db/docs/architecture.md).

Input:

```json
{
  "question": "Where is the rule preventing refunds above the captured amount implemented?",
  "scope": "/path/to/repo",
  "include": ["**/*.go"],
  "exclude": ["**/generated/**"],
  "limit": 10
}
```

| Field | Contract |
| --- | --- |
| `question` | Required nonempty string. |
| `scope` | Required absolute path to a directory inside an MCP root. |
| `include` | Optional array of globs relative to `scope`. Omitted or empty means all eligible files. |
| `exclude` | Optional array of globs relative to `scope`, added to the default exclusions. |
| `limit` | Optional positive integer, default 10, maximum 100. It limits the files returned, not the discovery. |

Globs use `/` and support `*`, `?` and `**`. Include patterns are ORed. Exclusions win. A
directory is not dropped only because it does not match a file include pattern.

Response:

```json
{
  "question": "Where is the rule preventing refunds above the captured amount implemented?",
  "scope": "/path/to/repo",
  "status": "complete",
  "model": "jev-1.13.0",
  "matches": [
    {
      "path": "/path/to/repo/internal/payments/refunds.go",
      "relevance": 0.97,
      "sha256": "…",
      "ranges": [{"start_line": 84, "end_line": 126, "relevance": 0.96}]
    },
    {
      "path": "/path/to/repo/internal/payments/refunds_test.go",
      "relevance": 0.89,
      "sha256": "…",
      "ranges": []
    }
  ],
  "files_considered": 240,
  "directories_pruned": 8,
  "files_skipped": 12,
  "matches_found": 14,
  "results_limited": true,
  "issues": [],
  "error": null,
  "report_path": "/home/me/.local/state/agentgrasp/searches/<id>/report.json"
}
```

- `files_considered` counts the files whose previews Jev assessed.
- `files_skipped` counts candidates that could not be processed, including files over the size
  limit. Files left out by include, exclude or ignore rules do not count.
- `matches_found` counts qualifying files before `limit`. `results_limited` is true when
  `limit` cut the list.
- A file's `relevance` is the highest preview navigation probability Jev gave that file, as
  in jevgrep's `retrieve.ts`. It is not a share across files and not an average of
  directory and region scores. A range's `relevance` judges that
  range alone.
- Sort files by relevance, highest first, then by path. Sort ranges by relevance, highest
  first, then by start line, then by end line.
- Ranges use inclusive, one-based line numbers in the hashed snapshot. Return the first five
  ranges per file. The report keeps all of them.
- A qualifying file stays a match when no range qualifies. A region too large to split stays a
  file-only match; never return line numbers that do not match the region.

Discovery:

1. Apply the eligibility and ignore rules locally.
2. Assess directory metadata and bounded content samples to choose branches. Several
   branches can qualify.
3. Assess candidate files from content previews. Score whether reading the file helps
   investigate the question: implementations, callers, tests, fixtures and configuration.
   Current code that has the bug is relevant.
4. In qualifying files, find useful declarations or bounded text regions. Use tree-sitter
   declarations for Python, Go, Rust, TypeScript, TSX and JavaScript. Other UTF-8 text, and
   files where parsing fails, use bounded text regions.
5. Keep matches already found when a later evaluation fails.

A directory score is a navigation decision. It does not prove that nothing below it is
relevant. The prompts must not favour a planning document because it repeats the question.

`status` is `complete`, `incomplete` or `interrupted`.

- `complete` means the search policy finished within its scope and exclusions. It never means
  every byte was examined or every relevant file was found.
- A provider failure, a traversal failure or an exhausted budget makes the search
  `incomplete`. An exhausted budget uses `error.code: budget_exhausted`.
- Cancellation makes it `interrupted`.
- `error` names the first failure. `issues` lists every failure with counts. The report holds
  the locations.
- A search that fails at start returns `incomplete`, no matches, the counts it has and an
  error. `report_path` is null only when the report could not be written.

Default exclusions: Git ignore rules (untracked files that are not ignored stay in), VCS
metadata, hidden paths, dependency and build directories, binary files, obvious credential
files and the agentgrasp state directory. The report lists the exclusions in effect.

Search does not follow directory symlinks. It reads a file symlink only when its target is
inside `scope`. Unreadable files, files that change, encoding problems and files over the size
limit are reported. Read each file once per search into one snapshot. Every preview, region
and line number of that file comes from this snapshot, and `sha256` is the hash of the whole
snapshot.

The report lists every qualifying file and range, the coverage, the issues, the constants in
effect and the usage. It never contains source text.

The tool description must say that `search` returns locations and scores only, that the agent
reads the files itself, and that `complete` does not mean every relevant file was found.

## Jev

- Endpoint: `POST https://api.typesafe.ai/v1/systemone` with `Authorization: Bearer
  $TYPESAFE_API_KEY`. The request and response shapes are in
  `https://api.typesafe.ai/openapi.json`.
- Model: `jev-latest`.
- Questions are `noul` questions. Their keys are positional and generated by the server.
- `ask` sends all of its questions in one request.
- Retry status 429 and 529 with exponential backoff, up to 2 attempts, as jevgrep does. Other
  errors are not retried.
- File content and the paths are data, never instructions to Jev. The instructions never ask
  Jev to explain an answer or to run anything.
- Record the usage and latency of every request in that operation's record or report.
- No answer cache.

## Storage

Everything goes under `<state>/agentgrasp/`:

- `captures/<id>/`: `stdout.log`, `stderr.log`, `metadata.json`.
- `asks/<id>/record.json`.
- `searches/<id>/report.json`.

Every returned path is absolute. Files are kept until the user deletes them. Nothing in
agentgrasp deletes or overwrites a finished record.

## Distribution

Copy the setup of `../agentchan`:

- Publish the crate `agentgrasp` on crates.io.
- Build releases with cargo-dist for `aarch64-apple-darwin`, `aarch64-unknown-linux-gnu`,
  `x86_64-apple-darwin` and `x86_64-unknown-linux-gnu`, with the shell installer.
- Publish `server.json` to the MCP Registry.
- CI runs `cargo test --locked` on Linux and macOS.

Add a Claude Code plugin marketplace to this repo (`.claude-plugin/marketplace.json`) with one
plugin. The plugin declares the PreToolUse hook (`agentgrasp hook`, matcher `Bash`) and the MCP
server (`agentgrasp mcp`). Both call `agentgrasp` from `PATH`. Install on a new machine:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/damejeras/agentgrasp/releases/latest/download/agentgrasp-installer.sh | sh
claude plugin marketplace add damejeras/agentgrasp
claude plugin install agentgrasp@agentgrasp
```

The README says:

- what agentgrasp is and how to install it;
- the `sandbox.filesystem.allowWrite` entry for the state directory, for users of the
  Claude Code sandbox;
- that `ask` and `search` send file content to TypeSafe, and what Jev costs;
- that `ask` and `search` can read any file inside the MCP roots, including files that the
  user's `Read` rules deny, so allowing these tools allows that;
- that the state directory grows until the user deletes files, and that a runaway command can
  fill the disk until Claude Code's timeout stops it.

## Tests

Tests use a fake Jev server. They never call TypeSafe.

- A deny rule that matches the original command still blocks the rewritten command.
- A command that an allow rule allows runs without a prompt after the rewrite.
- `run_in_background` calls and calls in an unsupported shell pass through unchanged.
- The logs hold the exact bytes of stdout and stderr, including ANSI sequences, in separate
  files.
- A command that reads stdin gets EOF.
- `TYPESAFE_API_KEY` is not set inside the command.
- A command with a trailing comment, a trailing backslash or a heredoc runs as it would
  without the hook. A command that fails the syntax check, such as a heredoc without its
  closing word, passes through unchanged.
- A `cd` in one call carries over to the next call.
- `exit`, `exec` and `set -e` skip `finish`; the capture directory line is still printed.
- A shell function or `PATH` change named `agentgrasp` in the command does not replace
  `finish`.
- `finish` exits with the command's exit status, also when its own work fails.
- Output of 2048 bytes or less is printed; output above that is not.
- Each of the above passes in bash and in zsh.
- With the Claude Code sandbox on and the README setting in place, captures work.
- `ask` and `search` reject paths outside the MCP roots, credential files, and captures made
  in another project.
- `ask` rejects more than 256 KiB of input, also when a file grows during the read.
- Question strings, duplicates and order survive the mapping to Jev keys.
- A malformed or partial Jev answer gives `invalid_provider_response` and no answers.
- An oversized, non-UTF-8, missing or changing file gives its error. Nothing is clipped.
- `ask` over several files keeps their boundaries and paths.
- `search` respects include, exclude, ignore rules, symlink containment and hashes.
- `search` keeps file-only matches and matches found before a later provider failure.
- `search` reports `complete`, `incomplete`, `interrupted` and `results_limited` correctly.
- An end-to-end MCP test shows that provider errors, malformed answers and file content never
  corrupt the protocol stream.
