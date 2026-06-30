# lunarfs-mcp

MCP server wrapping the [lunar](https://github.com/dev-dropbox/lunar) workspace engine.
Exposes six tools over STDIO so any MCP client can fork, mount, list, push, share,
and destroy workspaces without spawning a UI.

## Usage

```bash
npx lunarfs-mcp
```

The server listens on STDIO. Point your MCP client at it with transport `stdio`.

## Environment

| Variable | Default | Purpose |
| --- | --- | --- |
| `LUNAR_API_URL` | `http://127.0.0.1:8787` | Base URL for the lunar HTTP API (used by list and grant tools) |

## Tools

| Name | Channel | Description |
| --- | --- | --- |
| `fork_workspace` | CLI | Clone a workspace into an isolated ephemeral copy |
| `mount` | CLI | Attach a workspace to a local path |
| `list_workspaces` | HTTP | List workspaces, optionally filtered |
| `push` | CLI | Persist workspace state and produce a revision |
| `grant_access` | HTTP | Grant a user read/write/admin access |
| `destroy` | CLI | Permanently drop a workspace and all its state |

## Client Configuration

No global install required. `npx lunarfs-mcp` fetches and runs the server on demand.
All three clients use the same stdio transport: `command: npx`, `args: ["-y", "lunarfs-mcp"]`.

### Claude Code

Add to `.mcp.json` at your project root (project-scoped) or run the one-liner:

```bash
claude mcp add lunar npx -- -y lunarfs-mcp
```

Or edit `.mcp.json` directly:

```json
{
  "mcpServers": {
    "lunar": {
      "command": "npx",
      "args": ["-y", "lunarfs-mcp"]
    }
  }
}
```

### Cursor

Add to `.cursor/mcp.json` at your project root:

```json
{
  "mcpServers": {
    "lunar": {
      "command": "npx",
      "args": ["-y", "lunarfs-mcp"]
    }
  }
}
```

### Cline

Open `Cline > MCP Servers > Edit Config` in VS Code and add:

```json
{
  "mcpServers": {
    "lunar": {
      "command": "npx",
      "args": ["-y", "lunarfs-mcp"]
    }
  }
}
```

### Using the HTTP-backed tools (list, grant)

The `list_workspaces` and `grant_access` tools call the lunar HTTP API. If your
workflows use them, add the `env` block to any of the configs above:

```json
{
  "mcpServers": {
    "lunar": {
      "command": "npx",
      "args": ["-y", "lunarfs-mcp"],
      "env": {
        "LUNAR_API_URL": "http://127.0.0.1:8787"
      }
    }
  }
}
```

`LUNAR_API_URL` defaults to `http://127.0.0.1:8787` when omitted. The fork,
mount, push, and destroy tools use the local CLI and do not require this variable.

## License

Apache-2.0
