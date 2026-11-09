# Construct agent workspace — discussion outline

Started October 29, 2026. A working description and question queue, not an implementation plan.

## Working description

A Construct-first native desktop app for directing agents inside small, isolated virtual machines. Construct remains a fast, lean, independently usable Rust harness; ACP connects it to clients and supports delegated agents. The desktop app offers a polished workspace for conversations, agent activity, permissions, and reviewing work. Other ACP agents are supported alongside Construct.

The full harness and its tools run inside the guest. A host runtime manages guests and host resources. Host-controlled networking and isolation enforce the outer boundary independently of the guest agent's cooperation. Construct retains its own basic sandboxing for standalone use and defense in depth.

Confirmed by the user: **Construct-first, with other ACP agents supported.** The remaining choices below are proposals or open questions.

## Responsibilities to keep separate

| Piece | Responsibility |
| --- | --- |
| Construct harness | Agent loop, tools, context, learning, and its own run record. Independently usable through CLI/TUI and ACP. |
| Host runtime | VM creation, launch/stop, workspace storage, resource limits, ACP connection bridging, and controlled access to host resources. Its contract should be usable without the graphical app. |
| Isolation and networking | Guest/host separation and an enforced network path through host-controlled policy/proxies. No guest-controlled bypass route. |
| ACP connection | Agent sessions, prompts, streamed activity, cancellation, and cooperative tool permission requests. |
| Desktop UI | Present work and decisions; send requests to the runtime and agents. GPUI/GPUI Kit are candidates for implementation. |

These are responsibility boundaries. Whether the host runtime ships as a separate service or an app-managed process is still open.

## What the existing projects tell us

- Substrate already separates the renderer from the host daemon. Its agent loop lives in that daemon, while execution happens in the guest. The proposed change moves the complete harness into the guest and makes the host runtime independent of the UI. See `/home/brandon/git/substrate_v3/OVERVIEW.md`, especially its process diagram.
- Construct already serves ACP and consumes it for delegation. Its current ACP server does not forward Construct's own permission questions to the client; ADR 0016 explicitly leaves that open. That is an integration gap, not a missing ACP feature. See `/home/brandon/git/construct/docs/adr/0015-acp-both-ways.md`, ADR 0016, and `crates/construct/src/acp/server.rs`.
- Landlock is already kernel-enforced. The added VM boundary would contain the entire harness and its descendants; a host-side network policy must cover all guest traffic, including inference and tools.

## Protocol facts we can settle now

- **Tool permissions:** ACP supports `session/request_permission` and a selected-option/cancelled response. Standard approvals do not need a new protocol. [Official specification](https://agentclientprotocol.com/protocol/v1/tool-calls#requesting-permission)
- **Enforcement:** ACP approvals depend on the agent honoring them; ACP does not sandbox agents. VM/network policy needs independent enforcement. Lifecycle changes and proxy policy belong to the host runtime's own API/events. [Official security guidance](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/docs/protocol/v1/security.mdx)
- **Agent-to-agent:** An orchestrator can act as an ACP client to a worker. This provides delegation plumbing; discovery, peer messaging, routing, and coordination semantics remain application decisions. [Official architecture](https://agentclientprotocol.com/get-started/architecture)
- **UI motion:** GPUI Kit documents its own Base Motion system. The linked `open-gpui-motion` belongs to the separate Open GPUI ecosystem. Treat them as distinct candidates and verify compatibility before adding both. [Kit animation guide](https://gpui-kit.com/docs/animation/), [Open GPUI Motion](https://docs.rs/open-gpui-motion/latest/open_gpui_motion/)

Further protocol detail and sources: [ACP capabilities](acp-capabilities.md).

## Runtime reference to evaluate: Trail of Bits coop

Added October 30, 2026 at the user's request. [Trail of Bits coop](https://github.com/trailofbits/coop) appears to be the Rust agent sandbox they recalled. Evaluate it before selecting or building the host runtime, even if the outcome is design inspiration only.

Its Rust CLI manages VM environments for coding agents, with Firecracker on Linux and Lima on macOS. The repository carries an [Apache-2.0 license](https://github.com/trailofbits/coop/blob/main/LICENSE). Its own layer manages existing virtualization backends; distinguish that orchestration code from a custom-written hypervisor. [Repository README](https://github.com/trailofbits/coop/blob/main/README.md)

Evaluation questions:

- Can we use coop as a headless runtime behind the UI, reuse a narrow component, or learn enough from its design to simplify our own runtime?
- How does it launch the full agent inside the guest, persist workspaces, cancel processes, and recover after interruption?
- How are network routes, proxies, credentials, and host/guest filesystem exchange enforced on each platform?
- What dependencies, shipped images, binaries, license notices, and maintenance costs would adoption introduce? Assess them separately from the Rust harness's size target.

Capture source findings and license details in [the sandbox research note](trail-of-bits-sandbox.md). Adoption remains open.

## Question queue

Work through one at a time; later entries are not commitments.

1. **Product center — answered.** Construct-first; other ACP agents supported.
2. **Workspace/isolation unit — next.** Does a persistent project workspace own a VM and host several agents, or does each agent/task receive its own VM? This decides sharing, persistence, resource costs, and cross-agent containment.
3. **Runtime lifecycle.** What survives closing the window, reconnecting, restarting a guest, or cancelling a task? Who owns session and environment records?
4. **Host boundary.** How do files enter and changes return? Where do credentials live? Which network destinations are allowed? Which decisions are tool approvals versus host policy changes? Resolve Linux and macOS virtualization separately.
5. **Agent cooperation — answered.** [ADR 0011](docs/adr/0011-agent-cooperation.md): a Construct coordinator using optchat creates and prompts tasks; in-task delegation stays inside its task. The user owns every task and answers every approval; each worker is an ordinary task with its own branch.
6. **First complete user flow.** Define the smallest useful path from opening a project through launching Construct, directing work, handling approvals, reviewing changes, and stopping/resuming.
7. **UI and implementation.** Validate GPUI/Kit on target platforms, choose the visual direction and essential panels, then stage the work. Preserve measurable harness size/startup targets separately from UI/VM costs.
   - November 1, 2026: GPUIX (React bindings over GPUI, needs Node/Bun) set aside; spike started on plain GPUI pinned to Zed v0.233.10, no GPUI Kit. It works on Linux/Hyprland against `construct acp`; see README and AGENTS.md. Open: macOS check, whether drag feel and angled panels hold up at real scale.

Next action: choose the workspace/isolation unit; evaluate coop before committing to a VM stack or writing a new host runtime.

