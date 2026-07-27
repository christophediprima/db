# Querying over MCP

The Fluree **server** has a built-in [Model Context Protocol](https://modelcontextprotocol.io)
endpoint at **`/mcp`** — a thin, **read-only** query surface an AI agent can call as
"tools". It exposes exactly three tools:

| Tool | Purpose | Query language |
|------|---------|----------------|
| `get_data_model` | Discover a ledger's schema (classes, properties, counts) as markdown | — |
| `sparql_query`   | Run a SPARQL `SELECT` against a ledger | [SPARQL](sparql.md) |
| `fql_query`      | Run a Fluree **FQL (JSON-LD)** `SELECT` against a ledger | [JSON-LD Query](jsonld-query.md) |

> **Not the same as `fluree mcp serve`.** The [`fluree mcp`](../cli/mcp.md) CLI is a
> *stdio* server for IDE agents that exposes the Memory and Docs toolsets. This page is
> about the **HTTP `/mcp` endpoint of a running `fluree server`**, which exposes the three
> query tools above over the same port as the normal API. It is **off by default** — see
> [Enabling & authentication](#enabling--authentication).

Both `sparql_query` and `fql_query` return the same compact
[**Agent JSON envelope**](#the-agent-json-envelope), sized to a byte budget so results
don't overflow an agent's context window.

## Which tool to use

```
                    ┌─ need the schema first? ──────────────→  get_data_model
start a task ──────┤
                    └─ retrieving data?
                          ├─ plain graph query (traversal, count, filter)  →  sparql_query
                          └─ FULL-TEXT / KEYWORD search (BM25)             →  fql_query
```

The one hard rule: **BM25 full-text search is FQL-only.** A `f:searchText` block returns
nothing under SPARQL (the SPARQL parser treats `f:*` as ordinary predicates and silently
matches no triples). So any keyword / "search for…" / graph-aware-RAG retrieval **must**
go through `fql_query`. For everything else the two are interchangeable; pick the dialect
you're comfortable writing.

Always call `get_data_model` **first** so you know the classes and properties that exist
before you query.

## Enabling & authentication

The endpoint is off until you start the server with it on and name a trusted token issuer:

```bash
fluree server run \
  --mcp-enabled \
  --mcp-auth-trusted-issuer did:key:z6Mk...
```

Every `/mcp` request must carry a **signed bearer token** (an Ed25519 JWS). The token's
`iss` is a `did:key` that must be in the trusted-issuer list, and it should carry a
**`fluree.identity`** claim — the identity the query runs under (its read
[policy](../security/policy-in-queries.md) applies). Tokens are minted out-of-band by your
auth service (e.g. `fluree token create --identity <iri> …`).

See [MCP endpoint configuration](../operations/configuration.md#mcp-endpoint) for the full
flag/env reference (byte budget, query timeout, insecure dev mode) and
[Authentication](../security/authentication.md) for the token model.

## The tools

### `get_data_model`

| Argument | Type | Notes |
|----------|------|-------|
| `ledger` | string (required) | Ledger alias, e.g. `"mydb"` or `"mydb:main"` |

Returns the schema as markdown. Call it before querying.

### `sparql_query`

| Argument | Type | Notes |
|----------|------|-------|
| `ledger` | string (required) | Ledger alias. The ledger is set here, **not** via a SPARQL `FROM` clause. |
| `query`  | string (required) | A SPARQL **SELECT** query. `ASK` / `CONSTRUCT` / `DESCRIBE` / `UPDATE` are rejected. |
| `t`      | integer (optional) | Pin to a historical snapshot `t` for deterministic pagination (see below). |

### `fql_query`

| Argument | Type | Notes |
|----------|------|-------|
| `query`  | object (required) | A Fluree **FQL** query object. Must be SELECT-style (`select` / `selectOne` / `selectDistinct`) and must include a `from`. |

`fql_query` runs through the same execution path as the HTTP `POST /v1/fluree/query`
route, with the BM25 index provider wired in — so an embedded `f:searchText` block
executes in-process. The caller's identity is taken from the **token** and forced into the
query's `opts.identity`; any `opts.identity` in the body is ignored (it cannot be spoofed).

Rejected up front with a clear error:
- **No SELECT clause** — a node/graph FQL query has no solution-table shape, which the
  Agent JSON envelope requires. Add a `select` / `selectOne` / `selectDistinct`.
- **No `from`** — there is no ledger to resolve.
- **No resolved identity** — `fql_query` **fails closed** if the token carries neither
  `fluree.identity` nor `sub`. Because an FQL body can carry its own `opts.identity`,
  running identity-less would let a caller read under any identity it names; the tool
  refuses instead. (`sparql_query` needs no such guard — its body cannot supply an
  identity, so an identity-less call there merely runs unpoliced.)

## The Agent JSON envelope

Both query tools return a self-describing envelope (see
[Output formats → Agent JSON](output-formats.md#agent-json-format)):

```jsonc
{
  "schema":   { "?name": "xsd:string", "?score": "xsd:double" }, // per-variable datatype
  "rows":     [ { "?name": "Amazing Nature", "?score": 3.64 } ], // native JSON values
  "rowCount": 1,
  "t":        42,     // the snapshot's transaction time
  "hasMore":  false   // true → the result was truncated to the byte budget
}
```

When `hasMore` is `true`, the result was cut off at the byte budget
(`--mcp-agent-json-max-bytes`, default 32 KB) and a `message` field explains how to
continue:

- **`sparql_query`** — re-run with the **same `t`** (to stay on one snapshot), keep your
  `ORDER BY` (for stable page boundaries), and advance `OFFSET` by the returned `rowCount`
  (not by `LIMIT` — the byte budget can return fewer rows than `LIMIT`).
- **`fql_query`** — narrow the query, add a smaller `f:searchLimit` / FQL `limit`, or page
  with `limit` + `offset` (pin the snapshot with `from: "mydb:main@t:<t>"` for stable
  boundaries).

## Writing FQL queries

FQL (Fluree's JSON-LD query language) is less familiar than SPARQL, so here is the
practical subset you need for `fql_query`. This is a fast path — the full reference is
[JSON-LD Query](jsonld-query.md).

### Minimal shape

```json
{
  "@context": { "ex": "http://example.org/" },
  "from": "mydb:main",
  "where": [ { "@id": "?s", "ex:name": "?name" } ],
  "select": ["?s", "?name"]
}
```

- **`@context`** — prefix map. Define every prefix you use (`ex`, `as`, and `f` for the
  Fluree namespace `https://ns.flur.ee/db#` when you do full-text search).
- **`from`** — the ledger (or graph-source) alias. Add a snapshot pin with
  `"mydb:main@t:42"` for time travel / stable pagination.
- **`where`** — an array of patterns (see next). A single object is also accepted.
- **`select`** — the variables (`?var`) to return. Variants: `selectDistinct` (dedupe
  rows), `selectOne` (first row only — **not** for `fql_query`, which needs a table).

### `where` patterns

The common ones (see [Pattern Types](jsonld-query.md#pattern-types) for all):

```jsonc
// Object pattern — match a subject and bind its properties
{ "@id": "?post", "@type": "as:Article", "as:name": "?title" }

// Bind the type into a variable
{ "@id": "?s", "@type": "?type" }

// Join: reuse a variable across patterns (implicit AND across the array)
{ "@id": "?post", "as:attributedTo": "?actor" },
{ "@id": "?actor", "as:name": "?actorName" }
```

Filters, `optional`, `union`, property paths, `bind`, `values` all work — see
[Advanced Patterns](jsonld-query.md#advanced-patterns) and
[Filter Functions](jsonld-query.md#filter-functions). Modifiers: `orderBy`, `limit`,
`offset`, `groupBy`, `having` — see [Query Modifiers](jsonld-query.md#query-modifiers).
Aggregates (`count`, `sum`, …) — see [Aggregation Functions](jsonld-query.md#aggregation-functions).

> **SPARQL, not Virtuoso.** There is no `bif:` extension. For substring matching use
> standard `FILTER(CONTAINS(LCASE(?text), "…"))`; for real keyword ranking use BM25 search
> (below), which is far better.

### BM25 full-text search + join (the reason `fql_query` exists)

A [BM25 graph source](../indexing-and-search/bm25.md) is queried with an `f:searchText`
block inside `where`. It binds a document IRI and a relevance score, which you then **join**
with graph data in the *same* query — the graph-aware RAG pattern (retrieve by keyword,
then traverse the graph from the hits):

```json
{
  "@context": {
    "f": "https://ns.flur.ee/db#",
    "as": "https://www.w3.org/ns/activitystreams#"
  },
  "from": "silver:main",
  "where": [
    {
      "f:graphSource": "silver_bm25:main",
      "f:searchText": "nature",
      "f:searchLimit": 10,
      "f:searchResult": { "f:resultId": "?doc", "f:resultScore": "?score" }
    },
    { "@id": "?doc", "@type": "?type", "as:name": "?name" }
  ],
  "select": ["?doc", "?type", "?score", "?name"]
}
```

- **`f:graphSource`** — the BM25 index alias (a graph source registered alongside the
  ledger, e.g. `fluree bm25 create …`).
- **`f:searchText`** — the query terms.
- **`f:searchLimit`** — max hits to return from the index (do this *before* the join to
  keep the result small).
- **`f:searchResult`** — binds `f:resultId` → the matched IRI and `f:resultScore` → the
  BM25 score into your variables.
- The second pattern **joins** each hit IRI (`?doc`) back to the graph to pull its `@type`
  and `as:name`. Order by `?score` desc if you want ranked output.

Vector / semantic search uses the analogous `f:queryVector` block — see
[Graph Source Queries](jsonld-query.md#graph-source-queries) and
[Vector search](../indexing-and-search/vector-search.md).

## Calling the tools over the wire

A real MCP client does the `initialize` handshake for you. For a raw `curl`, send both the
JSON and SSE `Accept` types. The endpoint is streamable-HTTP JSON-RPC.

**`get_data_model`:**

```bash
curl -X POST http://localhost:8090/mcp \
  -H "Authorization: Bearer eyJhbGci..." \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
       "params":{"name":"get_data_model","arguments":{"ledger":"silver:main"}}}'
```

**`sparql_query`:**

```jsonc
{"jsonrpc":"2.0","id":2,"method":"tools/call",
 "params":{"name":"sparql_query","arguments":{
   "ledger":"silver:main",
   "query":"PREFIX as: <https://www.w3.org/ns/activitystreams#> SELECT ?s ?name WHERE { ?s a as:Article ; as:name ?name } LIMIT 50"}}}
```

**`fql_query`** — the `arguments.query` is the FQL object verbatim:

```jsonc
{"jsonrpc":"2.0","id":3,"method":"tools/call",
 "params":{"name":"fql_query","arguments":{
   "query":{
     "@context":{"f":"https://ns.flur.ee/db#","as":"https://www.w3.org/ns/activitystreams#"},
     "from":"silver:main",
     "where":[
       {"f:graphSource":"silver_bm25:main","f:searchText":"nature","f:searchLimit":10,
        "f:searchResult":{"f:resultId":"?doc","f:resultScore":"?score"}},
       {"@id":"?doc","@type":"?type","as:name":"?name"}
     ],
     "select":["?doc","?type","?score","?name"]
   }}}}
```

## Identity, policy, and per-user scoping

- The query runs under the token's `fluree.identity`, so content-level
  [read policy](../security/policy-in-queries.md) is enforced. `fql_query` forces this into
  `opts.identity` server-side — the query body cannot override it.
- The endpoint authorizes by **identity + policy**; the `ledger` / `from` a caller names is
  taken from the request. If you isolate tenants with a **ledger per (tenant, user)**, bind
  each caller to its own ledger at the tool boundary (the MCP client sets the `ledger`
  argument / FQL `from` from the authenticated principal, not from the model). Fluree's
  policy engine is graph-blind, so a ledger-per-user is the endorsed isolation axis — see
  [Policy in queries → multi-graph](../security/policy-in-queries.md).

## Errors you may see

| Message | Cause / fix |
|---------|-------------|
| `sparql_query supports SELECT queries only; …` | Sent `ASK` / `CONSTRUCT` / `DESCRIBE` / `UPDATE`. Use a `SELECT`. |
| `fql_query supports SELECT-style FQL only; …` | The FQL body has no `select` / `selectOne` / `selectDistinct`. |
| `fql_query requires a `from` clause …` | Add `from` naming the ledger or graph-source alias. |
| `fql_query requires an authenticated identity …` | The token carries no `fluree.identity`/`sub`. `fql_query` fails closed — mint a token with an identity. |
| `BM25 IndexSearch … (not configured)` | `f:searchText` against a `f:graphSource` that isn't a built BM25 index. Create/sync it first (`fluree bm25 create`). |
| `Bearer token required` / `Untrusted issuer` | Missing/invalid token, or its `iss` isn't in `--mcp-auth-trusted-issuer`. |

## See also

- [JSON-LD Query](jsonld-query.md) — the full FQL reference
- [SPARQL](sparql.md) — the full SPARQL reference
- [Output formats → Agent JSON](output-formats.md#agent-json-format) — the result envelope
- [BM25 full-text search](../indexing-and-search/bm25.md) · [Vector search](../indexing-and-search/vector-search.md)
- [MCP endpoint configuration](../operations/configuration.md#mcp-endpoint) — enabling & tuning
- [AI & agents](../ai/README.md) — how the `/mcp` endpoint fits an agent workflow
