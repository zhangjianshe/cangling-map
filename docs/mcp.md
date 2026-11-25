# Cangling Map MCP

`POST /mcp` provides a read-only Streamable HTTP MCP adapter over the existing
published 3D dataset catalog. It does not proxy arbitrary URLs, publish/delete data,
or control a user's Earth browser window.

## Connection

```json
{"type":"http","url":"https://train.cangling.cn/geoserver/mcp"}
```

Set `MAP_PUBLIC_BASE_URL=https://train.cangling.cn/geoserver/` on the server to
return absolute, version-pinned `tilesetUrl` values with the reverse proxy prefix.
Without it, results retain `tilesetPath` relative to the service root. Host and
forwarded headers are not used to construct resource URLs.
For local development use `MAP_PUBLIC_BASE_URL=http://127.0.0.1:4310/`.
This change is locally tested; deploy the new binary before using the public endpoint.

## Protocol and tools

Supports the 2025-03-26, 2025-06-18 and 2025-11-25 protocol versions (Keeper currently
negotiates 2025-06-18). Implements initialize, notifications/initialized,
notifications/cancelled, ping, tools/list and tools/call. Responses are JSON;
no protocol sessions, server-initiated requests or background SSE notifications.
GET returns 405. This does not claim support for the newer 2026 protocol revision.

- `search_3d_datasets`: optional query, bounds [west,south,east,north], limit (1–100).
- `get_3d_dataset`: required datasetId; absent/disabled datasets yield isError.

Results contain text JSON and structuredContent. Only enabled published datasets
are exposed, with version and geographic envelope. They do not include local
storage paths. The adapter calls the same discovery service as the HTTP API.
There is no building attribute query or editing capability in this version.

## HTTP boundary

Requests require application/json, Accept: application/json, text/event-stream,
and a supported MCP-Protocol-Version after initialization. Body limit: 16KiB.
Malformed requests and unknown methods use JSON-RPC errors; tool execution failures
use isError. Unknown tools/parameters are rejected.

Browser Origins are denied by default. Explicit trusted origins may be configured
as a comma-separated `MAP_MCP_ALLOWED_ORIGINS`; no wildcard CORS is applied to MCP.
Native clients with no Origin can read the same already-public catalog as HTTP.
No user login token is required for this public read-only data. Private datasets
would require an additional authentication/authorization design before exposure.
Bind local development to loopback. The standard resource endpoints retain their
existing CORS behavior for browser rendering.

## Verification

`cargo test -p map-server mcp::tests` and `python3 scripts/test-3dtiles.py` cover
protocol negotiation, discovery, calls, missing IDs, traversal, unknown methods,
Origin/version/Accept rejection, and existing publication semantics.
Keeper's `scripts/test-map-mcp.py` verifies the live client integration with a mock
model and an isolated account store.

Protocol reference: https://modelcontextprotocol.io/specification/2025-06-18/basic/transports
