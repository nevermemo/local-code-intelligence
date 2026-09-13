# LSP lifecycle and routing

- Define the executable, initialization options, workspace-root policy, supported language identifiers, timeout, and candidate limit as adapter data.
- Resolve the workspace root deterministically from the indexed canonical workspace. Do not use the editor's current folder as hidden state.
- Start lazily on the first applicable symbol/navigation request or retrieval channel use.
- Reuse one healthy process per workspace and configuration. Serialize JSON-RPC writes, correlate response IDs, drain notifications, and keep stderr from corrupting protocol output.
- Bound initialization and requests. On process exit, malformed protocol data, or timeout, discard the client and allow one fresh retry where the operation is safe.
- Stop child processes when the application shuts down or the owning state is dropped.
- LSP search is one candidate channel. Its failure must not suppress available semantic or lexical results.
- Direct navigation validates supported language before startup and reports tooling failures as MCP errors.
- Normalize UTF-16 character offsets according to LSP. Public input line numbers remain one-based.
- Map a location only to indexed chunks of that adapter's language and only inside the canonical workspace.
