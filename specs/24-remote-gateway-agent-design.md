# 24-remote-gateway-agent: Standalone Web Gateway and Agent Runtime

Status: draft v1
Owner: termstage
Last updated: 2026-05-26
Depends on: [10-browser-terminal-protocol-design.md](./10-browser-terminal-protocol-design.md),
[11-browser-terminal-runtime-design.md](./11-browser-terminal-runtime-design.md),
[20-browser-terminal-web-design.md](./20-browser-terminal-web-design.md),
[21-browser-terminal-public-exposure-design.md](./21-browser-terminal-public-exposure-design.md),
[23-local-remote-command-lease-design.md](./23-local-remote-command-lease-design.md)

## 1. Purpose

This spec defines how `termstage` evolves from an all-in-one local process into
a Teleport-style gateway/agent architecture that can run many independent
terminal sessions in EKS behind one public web endpoint.

The current embedded mode stays supported for local development and simple
presentations. Cluster mode adds:

- `termstage-web`: public browser gateway and session router.
- `termstage-agent`: private PTY/command worker that dials out to the gateway.

## 2. Product Requirement

Users need to run N interactive `termstage` sessions in Kubernetes without
creating N public Services, N Ingress rules, or exposing PTY workers directly.
Operators need one stable browser URL, central access control, and predictable
session lifecycle.

Success criteria:

| # | Requirement | Measure |
| --- | --- | --- |
| R1 | One gateway serves many terminal sessions. | 100 concurrent registered agents can be listed and routed by `sessionId` in a non-HA M0 test. |
| R2 | Agents are private. | Agents require only outbound connectivity to `termstage-web`; no per-agent public Service exists. |
| R3 | Existing embedded mode remains intact. | Current local CLI smoke tests continue to pass. |
| R4 | Browser protocol remains byte-compatible. | xterm.js still receives raw PTY bytes plus JSON control frames. |
| R5 | Command lifecycle is clear. | When an agent command exits, gateway notifies browsers and unregisters the session. |
| R6 | Browser users authenticate before terminal access. | `termstage-web` supports an OIDC authorization-code flow before serving terminal WebSockets. |

## 3. Architecture

```text
                          Public / Browser Network
                                      │
                                      ▼
                         ┌────────────────────────┐
                         │ Ingress / ALB          │
                         │ HTTPS / WSS            │
                         └───────────┬────────────┘
                                     │
                                     ▼
┌──────────────────────────────────────────────────────────────────┐
│ termstage-web                                                     │
│                                                                  │
│  ┌──────────────────┐    ┌──────────────────┐    ┌────────────┐ │
│  │ Browser Auth +   │───▶│ Session Router   │───▶│ Agent      │ │
│  │ HTTP/WS          │    │ - sessionId map  │    │ Tunnel Hub │ │
│  │ - static assets  │    │ - lease metadata │    │ - streams  │ │
│  │ - OIDC session   │◀───│ - authz policy   │◀───│ - liveness │ │
│  └──────────────────┘    └──────────────────┘    └─────┬──────┘ │
│                                                         │        │
│  ┌──────────────────┐                                  │        │
│  │ Audit/Event Sink │◀─────────────────────────────────┘        │
│  └──────────────────┘                                           │
└─────────────────────────────────────────────────────────────────┘
             ▲                                      ▲
             │ reverse tunnel                       │ reverse tunnel
             │ outbound from pod                    │ outbound from pod
             ▼                                      ▼
┌──────────────────────────────┐       ┌──────────────────────────────┐
│ termstage-agent              │       │ termstage-agent              │
│ sessionId=sess-a             │       │ sessionId=sess-b             │
│ - PTY runtime actor          │       │ - PTY runtime actor          │
│ - command/tmux child         │       │ - command/tmux child         │
│ - replay buffer owner        │       │ - replay buffer owner        │
└──────────────────────────────┘       └──────────────────────────────┘
```

The gateway never runs user commands in cluster mode. It terminates browser
connections, authenticates browser and agent connections, and routes framed
terminal traffic to the agent that owns the session.

## 4. Binaries and Modes

| Binary / mode | Responsibility | Intended environment |
| --- | --- | --- |
| `termstage` embedded | Current all-in-one local web + runtime process. | Developer laptop, demos, single-process smoke tests. |
| `termstage-web` | Public gateway, static assets, browser WS, session registry, agent tunnel hub. | EKS Deployment behind one Ingress/ALB. |
| `termstage-agent` | PTY runtime, command execution, replay buffer, process lifecycle. | EKS Job/Pod/Deployment in private subnets. |

The workspace can implement these as separate binaries in `apps/server` first.
Crate extraction is deferred until the module boundaries stabilize.

## 5. Network Protocol

### 5.1 Browser to Gateway

The browser protocol remains compatible with
[10-browser-terminal-protocol-design.md](./10-browser-terminal-protocol-design.md):

- binary frames carry terminal bytes;
- text frames carry JSON control;
- Host/Origin/token checks happen before WebSocket upgrade.

The browser URL gains a `sessionId` route or query parameter. The exact URL shape
is owned by [20-browser-terminal-web-design.md](./20-browser-terminal-web-design.md)
and [22-browser-terminal-base-path-design.md](./22-browser-terminal-base-path-design.md).

Before the browser can open a terminal WebSocket in gateway mode, it must have an
authenticated gateway session. The first target provider is Okta via OIDC
Authorization Code with PKCE:

```text
Browser                     termstage-web                 Okta / OIDC Provider
  │                              │                                  │
  │ 1. GET /sessions/sess-a ────▶│                                  │
  │                              │ 2. no gateway session            │
  │ 3. redirect /authorize ◀────│                                  │
  │                              │                                  │
  │ 4. authenticate ───────────────────────────────────────────────▶│
  │                              │                                  │
  │ 5. callback code/state ◀───────────────────────────────────────│
  │                              │                                  │
  │ 6. GET /oidc/callback ─────▶│                                  │
  │                              │ 7. exchange code + verifier ───▶│
  │                              │ 8. validate ID/access token ◀───│
  │                              │                                  │
  │ 9. set HttpOnly session ◀───│                                  │
  │                              │                                  │
  │10. WS /sessions/sess-a/ws ─▶│                                  │
  │                              │ 11. authorize user/session       │
```

Gateway sessions are server-side or signed/encrypted cookies with
`HttpOnly`, `Secure`, and `SameSite=Lax` attributes. Browser OIDC tokens are
never forwarded to `termstage-agent`; the gateway converts authenticated user
identity into an authorization decision for a `sessionId`.

### 5.2 Agent to Gateway

Agents establish an outbound, long-lived tunnel:

```text
Agent                         Gateway                       Browser
 │                              │                              │
 │ 1. Connect /agent/ws ───────▶│                              │
 │    join token / identity     │                              │
 │                              │ 2. Validate agent identity   │
 │                              │    register sessionId        │
 │                              │                              │
 │ 3. AgentReady ◀─────────────▶│                              │
 │    capabilities, size        │                              │
 │                              │                              │
 │                              │ 4. Browser WS attach ◀──────│
 │                              │    validate browser token    │
 │                              │                              │
 │ 5. AttachBrowser ───────────▶│                              │
 │                              │                              │
 │ 6. PTY bytes/control ◀══════▶│ 7. route frames ◀══════════▶│
 │                              │                              │
 │ 8. ProcessExited ──────────▶│ 9. notify browser ─────────▶│
 │                              │    unregister session        │
```

Agent tunnel frames use the same logical message categories as the in-process
runtime:

| Direction | Frame | Purpose |
| --- | --- | --- |
| Agent -> Gateway | `registerSession` | Claims `sessionId`, labels, command metadata, and capabilities. |
| Gateway -> Agent | `attachBrowser` | Starts routing one browser controller to the agent. |
| Gateway -> Agent | `browserInput` | Browser terminal bytes. |
| Gateway -> Agent | `browserResize` | Browser size proposal. |
| Agent -> Gateway | `ptyOutput` | Raw PTY output bytes. |
| Agent -> Gateway | `control` | `ready`, `leaseChanged`, `sizeChanged`, `processExited`, warnings, errors. |
| Either | `heartbeat` | Liveness, backpressure, and reconnect detection. |

M0 may encode this over one WebSocket per agent using JSON text frames for
control and binary frames for terminal bytes. If multiplexing multiple sessions
over one agent connection becomes necessary, add stream ids or move to HTTP/2
gRPC in a later milestone.

## 6. Session Registry

M0 registry is in memory inside one `termstage-web` replica:

```rust
struct SessionRegistry {
    sessions: DashMap<SessionId, AgentHandle>,
}
```

Session metadata:

| Field | Validation |
| --- | --- |
| `sessionId` | ASCII `[A-Za-z0-9_.-]`, 1..=64 bytes. |
| `agentId` | 128-bit random or Kubernetes pod UID hash, redacted in logs. |
| `namespace` | Optional Kubernetes namespace label, 1..=63 bytes. |
| `commandDisplay` | Server-generated safe display string, max 256 bytes. |
| `startedAt` | Gateway receive timestamp. |
| `lastHeartbeatAt` | Gateway monotonic heartbeat timestamp. |

HA registry is a later milestone. Redis/Postgres/Kubernetes CRD candidates must
be decided by `spike-session-registry-storage.md` before implementation.

## 7. Identity and Security

Cluster mode changes the trust boundary:

```text
Browser user ──OIDC login──▶ Gateway ──authenticated tunnel──▶ Agent ─▶ Command
```

Mandatory controls:

- Browser auth is OIDC-backed in gateway mode. Okta is the first target provider,
  but the implementation should depend on OIDC/OAuth2 standards, not Okta-only
  APIs.
- Local/embedded mode may keep generated URL tokens. Gateway mode must not rely
  on URL tokens as the primary user identity mechanism.
- Agent registration is separate from browser auth. A browser token must never
  register an agent, and a browser OIDC access token must never be accepted as an
  agent join credential.
- Agent join credentials are short-lived. The first EKS production target should
  validate Kubernetes service account projected tokens against the cluster OIDC
  issuer, then issue an agent session credential.
- Every agent register request is bound to allowed namespace/service account
  policy.
- The gateway rejects duplicate live `sessionId` registration unless the previous
  agent has timed out or explicitly disconnected.
- Terminal bytes, tokens, and command arguments marked secret are never logged.

This mirrors the Teleport lesson from
[../docs/research/survey-teleport-gateway-agent.md](../docs/research/survey-teleport-gateway-agent.md):
public proxy/gateway identity is separate from private agent enrollment.

## 8. Agent Sandbox Relationship

Agent Sandbox is relevant as a possible runtime substrate, not as a replacement
for `termstage-web`.

Agent Sandbox provides Kubernetes-native CRDs for isolated, stateful singleton
workloads:

| Agent Sandbox concept | Possible termstage use |
| --- | --- |
| `Sandbox` | One long-running `termstage-agent` environment with stable identity. |
| `SandboxTemplate` | Reusable agent runtime images, resource limits, storage, and isolation settings. |
| `SandboxClaim` | User/session-facing request to allocate an agent sandbox from a template. |
| `SandboxWarmPool` | Pre-warmed agents for faster interactive session startup. |
| gVisor/Kata runtime support | Stronger isolation for commands that run untrusted or LLM-generated code. |
| Persistent storage / hibernation | Resume long-lived coding-agent sessions without application-level state copy. |

The first gateway/agent implementation should not depend on Agent Sandbox. It
should run agents as ordinary Pods/Jobs first. A later milestone can add an
Agent Sandbox launcher:

```text
termstage-web API
      │ create session
      ▼
SandboxClaim ──▶ Agent Sandbox controller ──▶ Sandbox Pod
                                               │
                                               ▼
                                      termstage-agent reverse tunnel
```

Agent Sandbox affects scheduling, isolation, persistence, and warm-pool startup
latency. It does not define browser OIDC login, gateway session routing, terminal
byte framing, or agent tunnel authentication.

## 9. Lifecycle

Agent startup:

1. Validate CLI/config.
2. Start PTY runtime and child command.
3. Dial `termstage-web`.
4. Prove agent identity.
5. Register `sessionId`.
6. Replay/stream PTY output through gateway.

Agent shutdown:

1. Child exits or agent receives shutdown.
2. Agent sends `processExited` control if possible.
3. Gateway notifies browsers.
4. Gateway unregisters `sessionId`.
5. Agent closes tunnel and exits.

Gateway shutdown:

1. Stop accepting new browser and agent connections.
2. Send shutdown control to connected agents when policy allows.
3. Close browser sockets with a retryable gateway shutdown reason.
4. Drain audit/event queue.

## 10. Backpressure and Replay

Agent remains the owner of PTY replay in M0 because it already owns the PTY byte
stream. Gateway only buffers bounded per-connection mailboxes.

Rules:

- Agent -> gateway tunnel has bounded send queues.
- Gateway -> browser mailboxes stay bounded as in embedded mode.
- Slow browsers are closed without killing the agent command.
- Slow agent tunnels cause the session to be marked degraded; after timeout the
  gateway closes browser sessions and unregisters the agent.
- Replay caps remain explicit and configurable in later CLI/config work.

## 11. EKS Deployment Model

M0:

```text
termstage-web:
  kind: Deployment
  replicas: 1
  service: ClusterIP
  ingress: one public HTTPS/WSS endpoint

termstage-agent:
  kind: Job or Pod
  replicas: N independent sessions
  service: none
  network: outbound to termstage-web only
```

Later HA:

- Multiple `termstage-web` replicas behind sticky WebSocket routing or a shared
  registry plus tunnel ownership sharding.
- Gateway pods use readiness gates that fail when registry/tunnel hub cannot
  accept sessions.
- Agents reconnect with backoff and resume registration when a gateway restarts.

## 12. AGENTS.md Binding

- Error handling: `termstage-core` owns `thiserror` domain errors for tunnel,
  registry, and join failures; binaries add `anyhow::Context`.
- Async/concurrency: gateway registry and tunnel hub are actors or actor-owned
  maps. Use bounded channels. Do not wrap PTY/runtime state in shared locks.
- Type design: `SessionId`, `AgentId`, `AgentToken`, `TunnelFrame`, and
  `RegistryEntry` are explicit validated types.
- Safety/security: no `unsafe`; reject invalid external input at the tunnel and
  browser boundaries; cap string and frame sizes.
- Serialization: tunnel control frames use `serde`, `camelCase`, and
  `deny_unknown_fields`.
- Testing: unit tests for frame validation, registry state transitions, duplicate
  registration, and timeout cleanup; integration tests for one gateway plus two
  agents.
- Observability: structured `tracing` spans for agent join, browser attach,
  route start/stop, child exit, and backpressure; redact tokens and terminal bytes.
- Performance: zero-copy `Bytes` for terminal payloads where possible; bounded
  queues on every fan-out edge.
- Documentation: public tunnel protocol types document lifecycle and failure
  modes with `# Errors`.

## 13. Phased Delivery

| Phase | User-visible outcome | Engineering work |
| --- | --- | --- |
| M0 | One gateway and one agent can run a browser terminal over reverse tunnel. | Add tunnel protocol, `termstage-web`, `termstage-agent`, in-memory registry. |
| M1 | Many agents can register concurrently in one gateway. | Session routing, duplicate registration handling, bounded fan-out tests. |
| M2 | Browser users authenticate through Okta before terminal access. | OIDC Authorization Code with PKCE, gateway sessions, logout, WebSocket authorization. |
| M3 | EKS deployment works without public agent Services. | Kubernetes manifests/Helm, projected service account join spike, NetworkPolicy examples. |
| M4 | Gateway restart/reconnect is tolerable. | Agent reconnect, browser retry UX, session timeout semantics. |
| M5 | Agent Sandbox can launch isolated agents. | Optional `SandboxClaim` launcher, template selection, warm-pool integration. |
| M6 | HA gateway is possible. | Shared registry decision and implementation, sticky routing or tunnel sharding. |

## 14. Open Questions

- Should M0 agent tunnels use WebSocket or HTTP/2 gRPC?
- What is the minimum acceptable agent identity proof for non-EKS local testing?
- What exact OIDC library/session-store shape should the Rust gateway use for
  Okta, refresh, logout, and WebSocket authorization?
- Should replay remain agent-owned forever, or move to gateway for browser
  reconnect across agent reconnects?
- How should operators create a session: direct `termstage-agent` CLI, Kubernetes
  Job API, or gateway API that launches Jobs?
- Should Agent Sandbox become the preferred launcher after M0, or stay an
  optional integration for stronger isolation and stateful workspaces?

## 15. Cross-References

- Research: [../docs/research/survey-teleport-gateway-agent.md](../docs/research/survey-teleport-gateway-agent.md).
- Depends on: [10-browser-terminal-protocol-design.md](./10-browser-terminal-protocol-design.md),
  [11-browser-terminal-runtime-design.md](./11-browser-terminal-runtime-design.md),
  [20-browser-terminal-web-design.md](./20-browser-terminal-web-design.md),
  [21-browser-terminal-public-exposure-design.md](./21-browser-terminal-public-exposure-design.md),
  [23-local-remote-command-lease-design.md](./23-local-remote-command-lease-design.md).
- Consumed by future updates to:
  [50-browser-terminal-cli-design.md](./50-browser-terminal-cli-design.md),
  [70-browser-terminal-security-design.md](./70-browser-terminal-security-design.md),
  [72-browser-terminal-verification-plan.md](./72-browser-terminal-verification-plan.md),
  [90-browser-terminal-roadmap.md](./90-browser-terminal-roadmap.md),
  [91-browser-terminal-impl-plan.md](./91-browser-terminal-impl-plan.md).
