# Code Graph & Vector Intelligence

Official Website: **[https://knobyte.ai](https://knobyte.ai)**

Knobyte pairs deterministic Abstract Syntax Tree (AST) code graphs with embedded neuro-symbolic Datalog storage and dense vector embeddings. This allows coding agents to balance precise structural references with semantic similarity.

---

## 1. Deterministic AST Code Graph (Tree-sitter)

Knobyte embeds Tree-sitter grammars directly in pure Rust for:
- **Rust** (`tree-sitter-rust`)
- **TypeScript & TSX** (`tree-sitter-typescript`)
- **JavaScript & JSX** (`tree-sitter-javascript`)
- **Python** (`tree-sitter-python`)

### Extracted Entities
- **Nodes**: Functions, methods, structs, classes, interfaces, traits, enums, type aliases, and constants. Each node contains signature, start/end lines, body hash, and container scope.
- **Edges**: `calls`, `imports`, and `contains` relationships.

### Storage Architecture
- Code graph data is stored locally in `.knobyte/graph.db` using **SQLite with Write-Ahead Logging (WAL)**.
- Indexes:
  - `idx_nodes_name`: Fast symbol lookups.
  - `idx_edges_target`: Reverse dependency traversal (`who-calls`).
  - `idx_edges_source`: Forward call analysis.

### Structural Query Examples
```bash
# Locate definition
knobyte graph query where-defined start_sse_server

# Reverse call graph (callers)
knobyte graph query who-calls compute_body_hash

# Task neighborhood scoping
knobyte graph scope "session token renewal"
```

---

## 2. CozoDB Neuro-Symbolic Hybrid Engine

In addition to SQLite, Knobyte includes **CozoDB** — a 100% Rust hybrid relational, graph, and vector database powered by Sled storage.

### Schema Relations in `cozo.db`

```cozodatalog
:create code_nodes {
    id: String =>
    file_path: String,
    kind: String,
    name: String,
    start_line: Int,
    end_line: Int,
    body_hash: String,
    embedding: <F32; 128>
}

:create code_edges {
    source_id: String,
    target_id: String,
    kind: String =>
    file_path: String
}

::hnsw create code_nodes:node_vec {
    dim: 128,
    m: 16,
    dtype: F32,
    fields: [embedding],
    distance: Cosine,
    ef_construction: 64
}
```

---

## 3. 128-Dimensional Dense Vector Embeddings

Knobyte includes a built-in, deterministic, offline vector embedding generator requiring **0 external API calls** and working instantly on any platform:

- **Token Extraction**: Splits identifiers across `snake_case` and `camelCase`.
- **N-Gram Hashing**: Projects unigrams, word bigrams, and character trigrams into a 128-dimensional float space using SHA-256 hash seeds.
- **L2 Normalization**: Ensures all vectors are unit length ($\|v\|_2 = 1.0$), making cosine similarity equivalent to the dot product.

### Vector Search CLI

```bash
# Search code nodes
knobyte cozo search "authentication bearer token" --target code -k 5

# Search wiki markdown entities
knobyte cozo search "state management and caching" --target wiki -k 5
```

---

## 4. Graph Algorithms

### PageRank Centrality
Computes the structural importance of code symbols based on caller topology:
```bash
knobyte cozo pagerank --iterations 20 --damping 0.85
```

### Shortest Path
Finds the minimal call/import route between two functions:
```bash
knobyte cozo shortest-path "function:init_auth" "function:sign_token"
```

### Custom Datalog Queries
Execute arbitrary relational graph logic directly:
```bash
knobyte cozo query "?[kind, count(id)] := *code_nodes{kind, id}"
```
