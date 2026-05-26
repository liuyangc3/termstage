# Survey: Teleport-Style Gateway/Agent Architecture for Termstage

Status: Done · Owner: termstage · Date: 2026-05-26 · Kind: Web survey

## Question

What production-proven access architecture should `termstage` copy when splitting
the embedded web server into a standalone web gateway that can serve many
terminal-session workers in EKS?

## Sources

- Teleport Architecture, crawled 2026-05-26:
  https://goteleport.com/docs/reference/architecture/
- Teleport Proxy Service Architecture, crawled 2026-05-26:
  https://goteleport.com/docs/reference/architecture/proxy/
- Teleport Networking Reference, crawled 2026-05-26:
  https://goteleport.com/docs/reference/deployment/networking/
- Teleport Join Methods and Tokens, crawled 2026-05-26:
  https://goteleport.com/docs/reference/deployment/join-methods/
- Okta Authorization Code with PKCE guide, crawled 2026-05-26:
  https://developer.okta.com/docs/guides/implement-grant-type/authcodepkce/main/
- Kubernetes SIG Agent Sandbox documentation, crawled 2026-05-26:
  https://agent-sandbox.sigs.k8s.io/docs/

## Findings

1. Teleport separates the public access plane from the private resource plane. The
   control plane is Auth Service plus Proxy Service; the Proxy Service is the
   public-facing component and agents/resources remain in private networks.

2. Teleport agents establish reverse tunnels back to the Proxy Service. User
   traffic reaches the public proxy first, and the proxy forwards traffic over
   the reverse tunnel to the agent that owns the destination resource. This is
   the best fit for EKS because agent pods do not need public Services or
   per-session ingress.

3. Teleport's Proxy Service is identity-aware and owns the web UI, authenticated
   user entrypoint, protocol interception, reverse tunnel fan-out, and audit
   streaming. For termstage, this maps to a `termstage-web` gateway that owns
   browser auth, session routing, static assets, audit/event emission, and
   browser WebSockets.

4. Teleport's join model is explicit: an instance must prove identity before it
   receives certificates. It supports secret-based and delegated/cloud-provider
   join methods. For termstage on EKS, the first production join method should
   be Kubernetes service account projected tokens verified by the gateway, not a
   long-lived shared secret baked into pods.

5. Teleport supports running multiple services in one binary for small
   deployments, but the scalable pattern keeps the public proxy stable while
   private agents connect outward. Termstage should preserve the current
   all-in-one binary for local development while adding split `web` and `agent`
   modes for clusters.

6. Okta's Authorization Code with PKCE flow is the right browser-facing shape for
   `termstage-web`: browser users are redirected to Okta, Okta redirects back
   with an authorization code, and the gateway exchanges that code for tokens
   using the PKCE verifier. This belongs on the browser -> gateway boundary,
   not on the agent registration boundary.

7. Agent Sandbox is related but sits below the gateway/agent access plane. It is
   a Kubernetes-native API for isolated, stateful, singleton workloads using
   `Sandbox`, `SandboxTemplate`, `SandboxClaim`, and `SandboxWarmPool` CRDs. It
   also supports gVisor/Kata isolation, stable identity, persistent storage,
   hibernation/resume, and programmatic clients. Termstage can run
   `termstage-agent` inside Agent Sandbox sandboxes, but Agent Sandbox does not
   replace the termstage browser gateway or terminal byte-routing protocol.

## Recommended Termstage Shape

```text
                          Public / Browser Network
                                      │
                                      ▼
                         ┌────────────────────────┐
                         │ Ingress / ALB          │
                         │ TLS termination        │
                         └───────────┬────────────┘
                                     │ HTTPS/WSS
                                     ▼
┌──────────────────────────────────────────────────────────────────┐
│ termstage-web                                                     │
│ - static web UI                                                   │
│ - browser auth/token validation                                   │
│ - session registry                                                │
│ - browser WS <-> agent tunnel routing                             │
│ - audit/control events                                            │
└───────────────┬───────────────────────────────┬──────────────────┘
                │ outbound reverse tunnel        │ outbound reverse tunnel
                ▼                                ▼
     ┌──────────────────────┐         ┌──────────────────────┐
     │ termstage-agent pod  │         │ termstage-agent pod  │
     │ session A            │         │ session B            │
     │ - PTY runtime        │         │ - PTY runtime        │
     │ - child command      │         │ - child command      │
     └──────────────────────┘         └──────────────────────┘
```

## Decision

Use a Teleport-style reverse tunnel design:

- `termstage-web` is the only public service.
- `termstage-agent` dials out to `termstage-web` and owns the PTY child.
- The gateway routes browser sessions to agents by `sessionId`.
- Agent enrollment is explicit, short-lived, and identity-bound.
- Browser users authenticate to `termstage-web` through OIDC/OAuth2, with Okta
  as the first target provider.
- Agent Sandbox is an optional workload substrate for running isolated
  `termstage-agent` sessions, not the public access gateway.
- Keep the existing embedded mode as a compatibility/development path.

## What We Will Avoid

- Direct browser-to-agent networking or one Kubernetes Ingress per session.
- A shared long-lived registration token for every agent pod.
- Reusing browser OIDC tokens for agent registration.
- Having the gateway spawn commands itself in cluster mode. The gateway should
  remain an access/routing component, not the workload executor.
- Treating Agent Sandbox as a substitute for gateway auth/routing. It can manage
  where agents run, but it does not define browser access control or PTY routing.
- Making session registry durable in M0. A single gateway replica with in-memory
  registry is the smallest end-to-end slice; HA registry can come later.

## Open Questions

- `spike-kubernetes-service-account-join.md`: exact verifier shape for projected
  Kubernetes service account tokens from EKS.
- `spike-agent-tunnel-protocol.md`: WebSocket vs HTTP/2 gRPC stream for
  browser/agent multiplexing and backpressure.
- `spike-session-registry-storage.md`: whether Redis, Postgres, or Kubernetes API
  resources are the right HA registry backend.
- `spike-oidc-okta-session.md`: exact cookie/session shape for Okta OIDC login,
  refresh, logout, and browser WebSocket authorization.
- `spike-agent-sandbox-substrate.md`: whether `termstage-agent` should be
  launched as ordinary Pods/Jobs first or through Agent Sandbox `SandboxClaim`
  resources.
