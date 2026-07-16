# MCP for Agents

Simchain exposes a streamable HTTP MCP endpoint from the control plane so coding
agents can inspect and operate the live simnet without scraping the dashboard or
calling Docker directly.

Start the ordinary stack first:

```bash
docker compose up -d --build
```

The MCP endpoint is:

```text
http://localhost:8090/mcp
```

The endpoint requires the same bearer token as mutating API calls. The default
zero-config token is `simchain-control-dev-token`; if you set
`CONTROL_PLANE_API_TOKEN`, pass the same value to the MCP client.

## Connect Claude Code

```bash
claude mcp add --transport http simchain-control-plane \
  "http://localhost:8090/mcp" \
  --header "Authorization: Bearer ${SIMCHAIN_CONTROL_TOKEN:-simchain-control-dev-token}"
```

After registration, an MCP-capable agent can discover and call Simchain tools directly.
No Simchain plugin or skill is required. A skill can still be useful as extra guidance,
but the MCP tools are the actual integration.

## Connect Codex

Codex supports MCP servers through `codex mcp` and `config.toml`. A newly added MCP
server is loaded by new Codex sessions; an already-running Codex thread may need a
restart or a fresh session before the Simchain tools appear.

For the default local dev token, either export the token and use the CLI helper:

```bash
export SIMCHAIN_CONTROL_TOKEN="${SIMCHAIN_CONTROL_TOKEN:-simchain-control-dev-token}"
codex mcp add simchain-control-plane \
  --url http://localhost:8090/mcp \
  --bearer-token-env-var SIMCHAIN_CONTROL_TOKEN
```

Or add the server directly to `~/.codex/config.toml`:

```toml
[mcp_servers.simchain-control-plane]
url = "http://localhost:8090/mcp"
bearer_token_env_var = "SIMCHAIN_CONTROL_TOKEN"
```

Then start Codex from a shell where the token is present:

```bash
export SIMCHAIN_CONTROL_TOKEN="${SIMCHAIN_CONTROL_TOKEN:-simchain-control-dev-token}"
codex
```

For a purely local throwaway dev setup, a static header also works:

```toml
[mcp_servers.simchain-control-plane]
url = "http://localhost:8090/mcp"
http_headers = { "Authorization" = "Bearer simchain-control-dev-token" }
```

In the Codex TUI, use `/mcp` to inspect the active MCP servers and available tools.

## What Agents Can Do

The MCP surface is a thin adapter over the same control-plane service layer used by the
dashboard, HTTP API, and `simchainctl`.

Common tools:

```text
get_status
get_config
get_config_schema
set_config
set_mining_state
set_spam_state
start_reorg
start_partition
start_degrade
start_scenario
fund_addresses
get_faucet_status
get_faucet_transfer
get_job
list_jobs
abort_job
release_checkpoint
```

Example user prompts once the MCP server is connected:

```text
Show me the current chain height and whether spam is keeping up.
Pause the spammer.
Resume the spammer.
Set the mean block interval to 12 seconds.
Start a 3-block reorg on node3 and wait for the job to finish.
Partition node3 with 3 main blocks and 4 isolated blocks.
Fund this regtest address with 1 BTC from the faucet.
List recent jobs and explain any failures.
```

The agent should translate those into tool calls. For example, "pause the spammer"
maps to:

```json
{
  "tool": "set_spam_state",
  "args": {
    "state": "paused"
  }
}
```

And "resume the spammer" maps to:

```json
{
  "tool": "set_spam_state",
  "args": {
    "state": "running"
  }
}
```

## Browser Behavior

`/mcp` is not a human web page. If you type `http://localhost:8090/mcp` into Chrome,
the browser sends no `Authorization` header, so the control plane correctly returns
`401 unauthorized`.

You generally should not browse `/mcp` directly. Use an MCP client, such as Claude Code
registered with the command above.

Chrome DevTools can send authenticated API requests from the console, but that is only
useful for quick API checks, not for acting as an MCP client. Example:

```js
await fetch("http://localhost:8090/api/v1/spam/state", {
  method: "PUT",
  headers: {
    "Authorization": "Bearer simchain-control-dev-token",
    "Content-Type": "application/json"
  },
  body: JSON.stringify({ state: "paused" })
}).then(r => r.json())
```

The address bar cannot add bearer headers. A header-injection browser extension can
add one, but it still will not make `/mcp` pleasant to inspect because MCP is a client
protocol, not a rendered page.

## CLI Fallback

If an agent does not support MCP but has shell access in this repository, it can use
`simchainctl` instead:

```bash
cargo run -p simchainctl -- status
cargo run -p simchainctl -- spam pause
cargo run -p simchainctl -- spam resume
cargo run -p simchainctl -- config show
```

This is a fallback integration path. MCP is better when the agent supports it because
the tools, schemas, and operation descriptions are discoverable by the agent.

## Troubleshooting

Use `localhost` or `127.0.0.1`. The control plane rejects non-loopback Host headers to
avoid DNS rebinding against the page that exposes the browser token.

If the agent gets `401 unauthorized`, check that the MCP client sends:

```text
Authorization: Bearer simchain-control-dev-token
```

or, when using a custom token:

```text
Authorization: Bearer $CONTROL_PLANE_API_TOKEN
```

If you changed `CONTROL_PLANE_API_TOKEN` in `.env`, recreate the control-plane container
so the running service sees the new value.
