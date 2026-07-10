# Architecture

Documentation and process-flow diagrams for `claude-multi-proxy`, a local
reverse proxy that routes [Claude Code](https://claude.com/claude-code) requests
to multiple LLM providers with in-session `/model` switching.

All diagrams are [Mermaid](https://mermaid.js.org/) and render on GitHub, in
VS Code (with a Mermaid extension), and anywhere Mermaid is supported.

---

## 1. System context

Where the proxy sits between Claude Code and the upstream providers.

```mermaid
flowchart LR
    CC["Claude Code CLI<br/>ANTHROPIC_BASE_URL=<br/>http://localhost:8787"]

    subgraph proxy["claude-multi-proxy (127.0.0.1:8787)"]
        H1["POST /v1/messages"]
        H2["GET /health"]
    end

    subgraph providers["Upstream providers"]
        P1["deepseek<br/>api.deepseek.com"]
        P2["zai<br/>api.z.ai"]
        P3["kimi<br/>api.moonshot.ai"]
        P4["openrouter<br/>local y-router :8788"]
    end

    CFG["config.toml<br/>+ API-key env vars"]

    CC -- "Anthropic Messages API" --> H1
    CFG -. "loaded at startup" .-> proxy
    H1 -- "routed by default<br/>or /model command" --> P1 & P2 & P3 & P4
```

---

## 2. Module layout

How the crate is decomposed. `lib.rs` wires shared state; `main.rs` is the thin
binary entrypoint.

```mermaid
flowchart TD
    main["main.rs<br/>entrypoint: tracing, bind, serve"]
    lib["lib.rs<br/>build_state, log_key_presence"]
    config["config.rs<br/>Config / Provider, TOML load,<br/>API-key resolution"]
    proxy["proxy.rs<br/>router, /v1/messages, /health,<br/>AppState"]
    model["model_command.rs<br/>parse_and_strip /model cmd"]
    error["error.rs<br/>AppError -> HTTP response"]

    main --> lib
    main --> config
    main --> proxy
    lib --> config
    lib --> proxy
    proxy --> config
    proxy --> model
    proxy --> error
    config -. "ConfigError (thiserror)" .-> error
```

---

## 3. Startup sequence

What happens from process launch to a listening server.

```mermaid
sequenceDiagram
    participant OS as OS / shell
    participant Main as main.rs
    participant Cfg as config.rs
    participant Lib as lib.rs
    participant Axum as axum::serve

    OS->>Main: run binary
    Main->>Main: init tracing (RUST_LOG, default info)
    Main->>Cfg: Config::resolve_path() (PROXY_CONFIG or ./config.toml)
    Main->>Cfg: Config::load(path)
    alt file unreadable or invalid TOML
        Cfg-->>Main: Err(ConfigError)
        Main-->>OS: log error, ExitCode::FAILURE
    else parsed
        Cfg-->>Main: Config
        Main->>Lib: log_key_presence(&config)
        Note over Lib: warn for each provider whose<br/>API-key env var is unset
        Main->>Lib: build_state(config)
        Lib->>Lib: reqwest::Client with timeout
        Lib-->>Main: AppState { Arc<Config>, Client }
        Main->>Axum: bind host:port + serve(router)
        Axum-->>OS: listening on http://host:port
    end
```

---

## 4. Request processing flow — `POST /v1/messages`

The core routing and forwarding logic, including every error branch.

```mermaid
flowchart TD
    A["POST /v1/messages<br/>(raw body bytes)"] --> B{"parse body<br/>as JSON?"}
    B -- "no" --> E400a["AppError::InvalidJson<br/>-> 400"]
    B -- "yes" --> C["provider = default.provider<br/>model = body.model or default.model"]

    C --> D["parse_and_strip(body)<br/>(see diagram 5)"]
    D --> F{"/model command<br/>found?"}
    F -- "yes" --> G["override provider + model<br/>log 'model switch'"]
    F -- "no" --> H["keep defaults"]
    G --> I
    H --> I["write resolved model back into body"]

    I --> J{"provider in<br/>config?"}
    J -- "no" --> E400b["AppError::UnknownProvider<br/>-> 400"]
    J -- "yes" --> K{"API-key env<br/>var set?"}
    K -- "no" --> E500["AppError::MissingApiKey<br/>-> 500"]
    K -- "yes" --> L["build request:<br/>Authorization: Bearer key<br/>x-api-key: key<br/>JSON body"]

    L --> M{"upstream<br/>reachable?"}
    M -- "no (transport/timeout)" --> E502["AppError::Upstream<br/>-> 502"]
    M -- "yes" --> N{"status<br/>2xx?"}
    N -- "no" --> O["log warn<br/>(forward status as-is)"]
    N -- "yes" --> P["forward status"]
    O --> Q
    P --> Q["preserve status + content-type<br/>stream body through<br/>(SSE or JSON)"]
    Q --> R["response to Claude Code"]
```

---

## 5. `/model` command parsing — `parse_and_strip`

Detects `/model <provider>/<model>` in the first user message, reroutes, and
strips the command from the text. Handles both string content and the
array-of-content-blocks shape Claude Code emits.

```mermaid
flowchart TD
    S["parse_and_strip(body)"] --> A{"body.messages<br/>is an array?"}
    A -- "no" --> NONE["return None"]
    A -- "yes" --> B["scan messages in order"]

    B --> C{"next user<br/>message?"}
    C -- "none left" --> NONE
    C -- "yes" --> D{"content type?"}

    D -- "string" --> E["extract_command(text)"]
    D -- "array of blocks" --> F["scan text blocks<br/>extract_command(block.text)"]
    D -- "other" --> C

    E --> G{"valid<br/>provider/model?"}
    F --> G

    G -- "no" --> C
    G -- "yes" --> H["strip command token from text"]

    H --> I{"remaining text<br/>empty?"}
    I -- "yes (string)" --> J["remove whole message"]
    I -- "yes (block)" --> K["remove block;<br/>if message now empty, remove it"]
    I -- "no" --> L["replace text with remainder"]

    J --> R["return Some(provider, model)"]
    K --> R
    L --> R
```

### `extract_command` recognition rules

```mermaid
flowchart LR
    A["text"] --> B["trim_start"]
    B --> C{"starts with<br/>'/model '?"}
    C -- "no" --> X["None"]
    C -- "yes" --> D["identifier =<br/>first token after '/model '"]
    D --> E{"contains '/'<br/>as provider/model?"}
    E -- "no" --> X
    E -- "yes" --> F{"provider and model<br/>both non-empty?"}
    F -- "no" --> X
    F -- "yes" --> G["Some((provider, model),<br/>remainder text)"]
```

---

## 6. Response streaming

Both streaming (SSE) and non-streaming responses take the same unbuffered path —
the proxy never parses or re-serializes the provider's body.

```mermaid
sequenceDiagram
    participant CC as Claude Code
    participant PX as proxy.rs
    participant UP as Provider

    CC->>PX: POST /v1/messages (stream: true|false)
    PX->>UP: forward JSON body + auth headers
    UP-->>PX: response (status + headers)
    PX->>PX: copy status + content-type
    loop for each byte chunk
        UP-->>PX: chunk (bytes_stream)
        PX-->>CC: chunk (Body::from_stream)
    end
    Note over PX,CC: no buffering — SSE frames and<br/>plain JSON both pass straight through
```

---

## 7. Error mapping

Every `AppError` variant maps to one HTTP status and a JSON `{"error": ...}` body.
Upstream *HTTP* errors (4xx/5xx from the provider) are distinct: they are
forwarded through with the provider's real status, not remapped.

```mermaid
flowchart LR
    subgraph AppError
        I["InvalidJson"]
        U["UnknownProvider"]
        M["MissingApiKey"]
        S["Upstream (transport)"]
    end

    I --> R400["400 Bad Request"]
    U --> R400
    M --> R500["500 Internal Server Error"]
    S --> R502["502 Bad Gateway"]

    subgraph passthrough["Provider HTTP errors"]
        PE["4xx / 5xx from provider"]
    end
    PE --> FWD["forwarded as-is<br/>(logged at warn)"]
```

---

## Related documents

- [README.md](../README.md) — usage and setup
- [Design spec](superpowers/specs/2026-07-10-claude-multi-proxy-rust-design.md) — original design decisions
