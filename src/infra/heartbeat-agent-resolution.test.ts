import { describe, expect, it } from "vitest";
import { resolveHeartbeatAgents } from "../commands/doctor-heartbeat-legacy.js";
import type { OpenClawConfig } from "../config/types.openclaw.js";

describe("resolveHeartbeatAgents", () => {
  const systemOwnedConfig = {
    agents: {
      ownership: "explicit",
      entries: { ops: {}, main: {} },
      defaults: { systemAgent: { agentId: "ops" } },
    },
  } as OpenClawConfig;
  const ownerlessConfig = {
    agents: { ownership: "explicit", entries: { ops: {}, main: {} } },
  } as OpenClawConfig;

  it("enrolls the system agent when ambient heartbeat config is absent", () => {
    expect(resolveHeartbeatAgents(systemOwnedConfig)).toEqual([
      { agentId: "ops", heartbeat: undefined },
    ]);
  });

  it("disables ambient heartbeats when an explicit multi-agent roster has no owner", () => {
    expect(resolveHeartbeatAgents(ownerlessConfig)).toEqual([]);
  });

  it("refreshes membership after mutable heartbeat enrollment changes", () => {
    const entries: Record<string, { heartbeat?: { every: string } }> = {
      main: {},
      ops: { heartbeat: { every: "0m" } },
    };
    const cfg: OpenClawConfig = { agents: { ownership: "explicit", entries } };
    expect(resolveHeartbeatAgents(cfg).map((agent) => agent.agentId)).toEqual(["ops"]);
    entries.ops = { heartbeat: undefined };
    expect(resolveHeartbeatAgents(cfg)).toEqual([]);
    entries.ops = { heartbeat: { every: "5m" } };
    expect(resolveHeartbeatAgents(cfg).map((agent) => agent.agentId)).toEqual(["ops"]);
  });

  it("preserves explicit list order and duplicate IDs with the first normalized config", () => {
    const cfg: OpenClawConfig = {
      agents: {
        ownership: "explicit",
        defaults: { heartbeat: { agentId: "main", target: "owner" } },
        list: [
          { id: "OPS", heartbeat: { every: "0m" } },
          { id: "ops", heartbeat: { every: "5m" } },
          { id: "main" },
        ],
      },
    };
    expect(resolveHeartbeatAgents(cfg)).toEqual([
      { agentId: "ops", heartbeat: { agentId: "main", target: "owner", every: "0m" } },
      { agentId: "ops", heartbeat: { agentId: "main", target: "owner", every: "0m" } },
    ]);
  });

  it.each([
    {
      name: "explicit heartbeat owner",
      cfg: {
        agents: {
          ownership: "explicit",
          entries: { main: {}, ops: {} },
          defaults: { heartbeat: { agentId: "ops" } },
        },
      } as OpenClawConfig,
      expectedAgentIds: ["ops"],
    },
    {
      name: "legacy default marker",
      cfg: {
        agents: { entries: { main: { default: true }, ops: {} } },
      } as OpenClawConfig,
      expectedAgentIds: ["main"],
    },
    {
      name: "sole agent",
      cfg: { agents: { ownership: "explicit", entries: { solo: {} } } } as OpenClawConfig,
      expectedAgentIds: ["solo"],
    },
    {
      name: "per-agent enrollment takes precedence over the default heartbeat owner",
      cfg: {
        agents: {
          ownership: "explicit",
          entries: { main: {}, ops: { heartbeat: { every: "30m" } } },
          defaults: { heartbeat: { agentId: "main" } },
        },
      } as OpenClawConfig,
      expectedAgentIds: ["ops"],
    },
    {
      name: "broadcast heartbeat defaults",
      cfg: {
        agents: {
          ownership: "explicit",
          entries: { main: {}, ops: {} },
          defaults: { heartbeat: { every: "30m" } },
        },
      } as OpenClawConfig,
      expectedAgentIds: ["main", "ops"],
    },
  ])("enrolls exactly the runnable agents for the $name config", ({ cfg, expectedAgentIds }) => {
    const agents = resolveHeartbeatAgents(cfg);
    expect(agents.map((agent) => agent.agentId)).toEqual(expectedAgentIds);
  });

  it.each([
    ["entries", "explicit"],
    ["entries", "defaults"],
    ["list", "explicit"],
    ["list", "defaults"],
  ] as const)("enrolls a %s %s fleet with linear reads and fresh config", (form, enrollment) => {
    const size = 64;
    const rows = Array.from({ length: size }, (_, index) => ({
      id: `agent-${index}`,
      ...(enrollment === "explicit" ? { heartbeat: { every: "45m" } } : {}),
    }));
    const entries = Object.fromEntries(rows.map(({ id, ...entry }) => [id, entry]));
    let reads = 0;
    const observe = <T extends object>(roster: T): T =>
      new Proxy(roster, {
        get(target, key, receiver) {
          if (typeof key === "string" && (key.startsWith("agent-") || /^\d+$/.test(key))) {
            reads += 1;
          }
          return Reflect.get(target, key, receiver);
        },
      });
    const defaults = { heartbeat: { every: "30m", target: "owner" as const } };
    const cfg: OpenClawConfig = {
      agents: {
        ownership: "explicit",
        defaults,
        ...(form === "entries" ? { entries: observe(entries) } : { list: observe(rows) }),
      },
    };
    const expected = rows.map(({ id }) => ({
      agentId: id,
      heartbeat: { every: enrollment === "explicit" ? "45m" : "30m", target: "owner" },
    }));

    expect(resolveHeartbeatAgents(cfg)).toEqual(expected);
    // Bound actual entry reads, allowing several passes while rejecting a scan per agent.
    expect(reads).toBeLessThanOrEqual(size * 4);

    defaults.heartbeat.every = "20m";
    if (form === "entries") {
      delete entries[`agent-${size - 1}`];
    } else {
      rows.pop();
    }
    reads = 0;
    expect(resolveHeartbeatAgents(cfg)).toEqual(
      expected.slice(0, -1).map(({ agentId }) => ({
        agentId,
        heartbeat: { every: enrollment === "explicit" ? "45m" : "20m", target: "owner" },
      })),
    );
    expect(reads).toBeLessThanOrEqual(size * 4);
  });
});
