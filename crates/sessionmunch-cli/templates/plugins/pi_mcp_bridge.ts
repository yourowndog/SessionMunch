
// ---- MCP bridge ------------------------------------------------------------
const MCP_SERVER = deriveMcpServer(SERVER);
const MCP_REQUEST_TIMEOUT_MS = 10000;
let mcpRequestId = 0;

function deriveMcpServer(server: string): string {
  const trimmed = server.replace(/\/+$/, "");
  return trimmed.endsWith("/mcp") ? trimmed : `${trimmed}/mcp`;
}

function mcpSessionId(ctx: any): string | undefined {
  const id = sessionID(ctx) ?? ctx?.sessionId ?? ctx?.sessionID ?? ctx?.session?.id;
  return typeof id === "string" && id.length > 0 ? id : undefined;
}

function mcpSignal(signal?: AbortSignal): AbortSignal | undefined {
  const timeout = timeoutSignal(MCP_REQUEST_TIMEOUT_MS);
  if (!signal) return timeout;
  if (!timeout) return signal;
  const anyFactory = (AbortSignal as unknown as { any?: (signals: AbortSignal[]) => AbortSignal }).any;
  return anyFactory ? anyFactory([signal, timeout]) : timeout;
}

async function mcpRpc(method: string, params?: unknown, ctx?: any, signal?: AbortSignal): Promise<any> {
  const id = ++mcpRequestId;
  const headers: Record<string, string> = {
    "Content-Type": "application/json",
    "Accept": "application/json, text/event-stream",
    ...authHeaders(),
  };
  const session = mcpSessionId(ctx);
  if (session) {
    headers["X-Memory-Actor-Session-Id"] = session;
    headers["Mcp-Session-Id"] = session;
  }
  const response = await fetch(MCP_SERVER, {
    method: "POST",
    headers,
    body: JSON.stringify({ jsonrpc: "2.0", id, method, params: params ?? {} }),
    signal: mcpSignal(signal),
  });
  if (!response.ok) throw new Error(`sessionmunch MCP ${method} failed: HTTP ${response.status}`);
  const payload = await response.json();
  if (payload?.error) throw new Error(`sessionmunch MCP ${method} failed: ${payload.error.message ?? JSON.stringify(payload.error)}`);
  if (payload?.result?.isError) throw new Error(`sessionmunch MCP ${method} returned isError`);
  return payload?.result;
}

function toolInputSchema(tool: any): any {
  return tool?.inputSchema ?? { type: "object", additionalProperties: true };
}

async function bootstrapMcpBridge(pi: any): Promise<void> {
  try {
    await mcpRpc("initialize", {
      protocolVersion: "2025-03-26",
      capabilities: {},
      clientInfo: { name: "sessionmunch-pi-extension", version: "0.0.0" },
    });
    try { await mcpRpc("notifications/initialized"); } catch (_e) {}
    const listed = await mcpRpc("tools/list");
    for (const tool of listed?.tools ?? []) {
      try {
        pi.registerTool({
          name: tool.name,
          label: tool.name,
          description: tool.description,
          parameters: toolInputSchema(tool),
          execute: async (_toolCallId: string, params: unknown, signal?: AbortSignal, _onUpdate?: unknown, ctx?: any) => {
            const result = await mcpRpc("tools/call", { name: tool.name, arguments: params ?? {} }, ctx, signal);
            return { content: result?.content ?? [], details: result };
          },
        });
      } catch (_e) {
        // Duplicate registration or tool-shape mismatch must not break lifecycle capture.
      }
    }
  } catch (_e) {
    // MCP bridge is best-effort; extension load and lifecycle capture must survive.
  }
}
