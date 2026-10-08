import path from "node:path";
import {
  afterAll,
  afterEach,
  beforeEach,
  describe,
  expect,
  it,
  vi,
  type MockInstance,
} from "vitest";
import { createDeferred } from "../../test/helpers/promise.js";
import type { InternalSessionEntry as SessionEntry } from "../config/sessions.js";
import * as sessionAccessor from "../config/sessions/session-accessor.js";
import { loadSessionEntry, replaceSessionEntry } from "../config/sessions/session-accessor.js";
import * as sessionEntryReadRuntime from "../config/sessions/session-entry-read-runtime.js";
import {
  emitAgentEvent,
  getAgentEventLifecycleGeneration,
  rotateAgentEventLifecycleGeneration,
} from "../infra/agent-events.js";
import { claimAgentRunContext, releaseAgentRunContext } from "../infra/agent-run-registry.js";
import { useSessionStoreTempDirs } from "../test-utils/session-state-cleanup.js";
import { LIVE_RUN_FENCE_SETTLE_GRACE_MS } from "./agent-command-live-run-fence.js";
import { runWithAgentCommandRecoveryOwner } from "./agent-command-recovery-owner.js";
import type { AgentCommandOpts } from "./command/types.js";
import {
  clearActiveEmbeddedRun,
  setActiveEmbeddedRun,
  type EmbeddedAgentQueueHandle,
} from "./embedded-agent-runner/runs.js";

const sessionDirs = useSessionStoreTempDirs(afterAll, "openclaw-agent-command-live-run-");
const sessionKey = "agent:main:main";

afterEach(() => {
  vi.restoreAllMocks();
  vi.clearAllMocks();
  vi.useRealTimers();
});

function createTarget() {
  const storePath = path.join(sessionDirs.make(), "sessions.json");
  return {
    sessionAgentId: "main",
    isNewSession: false,
    sessionId: "session-1",
    sessionKey,
    storePath,
  };
}

async function write(target: ReturnType<typeof createTarget>, entry: Partial<SessionEntry>) {
  await replaceSessionEntry(target, { sessionId: target.sessionId, updatedAt: 100, ...entry });
}

function read(target: ReturnType<typeof createTarget>) {
  return loadSessionEntry(target);
}

function execute<T extends ReturnType<typeof createTarget>>(
  target: T,
  overrides: Partial<Parameters<typeof runWithAgentCommandRecoveryOwner<T, unknown>>[0]> = {},
) {
  return runWithAgentCommandRecoveryOwner({
    lifecycleGeneration: getAgentEventLifecycleGeneration(),
    mode: "claim",
    opts: {} as AgentCommandOpts,
    prepare: async () => target,
    run: async () => "ran",
    ...overrides,
  });
}

describe("agent command admission while another turn is still running on the session", () => {
  const activeRunId = "active-turn";
  let readWorker: MockInstance<typeof sessionEntryReadRuntime.readSessionEntryReadOnlyInWorker>;

  beforeEach(() => {
    readWorker = vi.spyOn(sessionEntryReadRuntime, "readSessionEntryReadOnlyInWorker");
  });

  // The running turn's own lifecycle start records the fence on the session row. While the
  // model streams it holds a runtime handle; while a tool runs it holds a run context claim.
  function startActiveTurn(
    lifecycleGeneration: string,
    via: "runtime handle" | "run context" = "run context",
  ) {
    if (via === "runtime handle") {
      const handle = {
        runId: activeRunId,
        queueMessage: async () => {},
        isStreaming: () => true,
        isCompacting: () => false,
        abort: () => {},
      } as unknown as EmbeddedAgentQueueHandle;
      setActiveEmbeddedRun("session-1", handle, sessionKey);
      return () => clearActiveEmbeddedRun("session-1", handle, sessionKey);
    }
    const claimId = claimAgentRunContext(
      activeRunId,
      { sessionKey, lifecycleGeneration },
      { trackOwner: true },
    );
    return () => releaseAgentRunContext(activeRunId, claimId);
  }

  function activeFence(lifecycleGeneration: string): Partial<SessionEntry> {
    return {
      abortedLastRun: false,
      lifecycleRunId: activeRunId,
      restartRecoveryRuns: [{ runId: activeRunId, lifecycleGeneration }],
    };
  }

  async function finishActiveTurn(
    target: ReturnType<typeof createTarget>,
    clear: () => void,
    phase: "end" | "error" = "end",
  ) {
    emitAgentEvent({ runId: activeRunId, stream: "lifecycle", data: { phase } });
    clear();
    await write(target, { updatedAt: 300, status: phase === "end" ? "done" : "failed" });
  }

  async function settleTicks() {
    for (let index = 0; index < 20; index += 1) {
      await new Promise<void>((resolve) => {
        setImmediate(resolve);
      });
    }
  }

  // Fake timers also freeze the worker transport; these cases read the row in-process instead.
  function readRowInProcess() {
    vi.spyOn(sessionEntryReadRuntime, "readSessionEntryReadOnlyInWorker").mockImplementation(
      async (scope) => loadSessionEntry(scope),
    );
  }

  async function waitFor(condition: () => boolean) {
    // A cold worker start can take seconds on a loaded runner.
    for (let index = 0; index < 3_000 && !condition(); index += 1) {
      await new Promise<void>((resolve) => {
        setTimeout(resolve, 10);
      });
    }
    expect(condition()).toBe(true);
  }

  it.each([
    ["run context", false],
    ["runtime handle", false],
    ["runtime handle", true],
  ] as const)(
    "queues behind the active turn holding its %s (own delivery claim: %s) and runs once after it settles",
    async (via, ownDeliveryClaim) => {
      const target = { ...createTarget(), timeoutMs: 60_000 };
      const lifecycleGeneration = getAgentEventLifecycleGeneration();
      await write(target, {
        ...activeFence(lifecycleGeneration),
        // Operator and channel turns record their own delivery claim beside their fence.
        ...(ownDeliveryClaim ? { restartRecoveryDeliveryRunId: activeRunId } : {}),
      });
      const clear = startActiveTurn(lifecycleGeneration, via);
      const run = vi.fn(async () => "ran after active turn");
      const prepare = vi.fn(async () => ({
        ...target,
        runLease: { release: vi.fn(async () => {}) },
      }));
      try {
        const queued = execute(target, {
          lifecycleGeneration,
          opts: { runId: "queued-turn" } as AgentCommandOpts,
          prepare,
          run,
        });
        await settleTicks();
        expect(run).not.toHaveBeenCalled();

        await finishActiveTurn(target, clear);
        await expect(queued).resolves.toBe("ran after active turn");
        expect(run).toHaveBeenCalledOnce();
        expect(prepare).toHaveBeenCalledTimes(2);
        for (const result of prepare.mock.results) {
          expect((await result.value).runLease.release).toHaveBeenCalledOnce();
        }
      } finally {
        clear();
      }
    },
  );

  it.each<{ name: string; fields: Partial<SessionEntry> }>([
    {
      name: "an assigned recovery cycle",
      fields: { mainRestartRecovery: { cycleId: "cycle-1", revision: 3, chargedAttempts: 1 } },
    },
    {
      name: "an outstanding recovery delivery",
      fields: { restartRecoveryDeliveryRunId: "delivery" },
    },
  ])("still rejects immediately when the row carries $name", async ({ fields }) => {
    const target = { ...createTarget(), timeoutMs: 60_000 };
    const lifecycleGeneration = getAgentEventLifecycleGeneration();
    await write(target, { ...activeFence(lifecycleGeneration), ...fields });
    const clear = startActiveTurn(lifecycleGeneration);
    const run = vi.fn();
    try {
      await expect(
        execute(target, {
          lifecycleGeneration,
          opts: { runId: "queued-turn" } as AgentCommandOpts,
          run,
        }),
      ).rejects.toMatchObject({ code: "SESSION_WORK_START_CHANGED" });
      expect(run).not.toHaveBeenCalled();
    } finally {
      clear();
    }
  });

  it("keeps FIFO order for two waiters when the earlier one prepares again slowly", async () => {
    const target = { ...createTarget(), timeoutMs: 60_000 };
    const lifecycleGeneration = getAgentEventLifecycleGeneration();
    await write(target, activeFence(lifecycleGeneration));
    const clear = startActiveTurn(lifecycleGeneration);
    const order: string[] = [];
    const slowSecondPreparation = createDeferred();
    const preparations = { first: 0, second: 0 };
    const waiter = (name: "first" | "second") =>
      execute(target, {
        lifecycleGeneration,
        opts: { runId: `${name}-queued-turn` } as AgentCommandOpts,
        prepare: async () => {
          preparations[name] += 1;
          if (name === "first" && preparations.first === 2) {
            await slowSecondPreparation.promise;
          }
          return target;
        },
        run: async () => {
          order.push(name);
          return name;
        },
      });
    try {
      const first = waiter("first");
      // The fence read resolved and the wait's own first check started: the first waiter waits.
      await waitFor(() => readWorker.mock.calls.length >= 2);
      const second = waiter("second");
      await settleTicks();
      expect(order).toEqual([]);

      await finishActiveTurn(target, clear);
      await waitFor(() => preparations.first === 2);
      // The later command stays queued behind the earlier one while it prepares again.
      await settleTicks();
      expect(order).toEqual([]);

      slowSecondPreparation.resolve();
      await expect(first).resolves.toBe("first");
      await expect(second).resolves.toBe("second");
      expect(order).toEqual(["first", "second"]);
    } finally {
      clear();
    }
  });

  it("reads the session row only through the worker owner while it waits", async () => {
    const target = { ...createTarget(), timeoutMs: 60_000 };
    const lifecycleGeneration = getAgentEventLifecycleGeneration();
    await write(target, activeFence(lifecycleGeneration));
    const clear = startActiveTurn(lifecycleGeneration);
    try {
      const queued = execute(target, {
        lifecycleGeneration,
        opts: { runId: "queued-turn" } as AgentCommandOpts,
      });
      await waitFor(() => readWorker.mock.calls.length >= 2);
      const syncRead = vi.spyOn(sessionAccessor, "loadSessionEntry");
      const workerReads = readWorker.mock.calls.length;
      // An unrelated row change wakes the waiter; it rechecks without a main-thread read.
      await write(target, { ...activeFence(lifecycleGeneration), updatedAt: 250 });
      await waitFor(() => readWorker.mock.calls.length > workerReads);
      expect(syncRead).not.toHaveBeenCalled();
      syncRead.mockRestore();

      await finishActiveTurn(target, clear);
      await expect(queued).resolves.toBe("ran");
      expect(readWorker).toHaveBeenCalledWith(
        expect.objectContaining({ sessionKey, storePath: target.storePath }),
      );
    } finally {
      clear();
    }
  });

  it("still rejects a retained fence whose run is no longer executing", async () => {
    // A yielded or abandoned run keeps its fence without a live execution; that row stays fenced.
    const target = { ...createTarget(), timeoutMs: 60_000 };
    const lifecycleGeneration = getAgentEventLifecycleGeneration();
    await write(target, {
      ...activeFence(lifecycleGeneration),
      status: undefined,
      endedAt: 1000,
    });
    const run = vi.fn();
    await expect(
      execute(target, {
        lifecycleGeneration,
        opts: { runId: "queued-turn" } as AgentCommandOpts,
        run,
      }),
    ).rejects.toMatchObject({ code: "SESSION_WORK_START_CHANGED" });
    expect(run).not.toHaveBeenCalled();
  });

  it("stops waiting when the queued turn is aborted", async () => {
    const target = { ...createTarget(), timeoutMs: 60_000 };
    const lifecycleGeneration = getAgentEventLifecycleGeneration();
    await write(target, activeFence(lifecycleGeneration));
    const clear = startActiveTurn(lifecycleGeneration);
    const controller = new AbortController();
    const release = vi.fn(async () => {});
    const run = vi.fn();
    try {
      const queued = execute(target, {
        lifecycleGeneration,
        opts: { runId: "queued-turn", abortSignal: controller.signal } as AgentCommandOpts,
        prepare: async () => ({ ...target, runLease: { release } }),
        run,
      });
      void queued.catch(() => {});
      await settleTicks();
      controller.abort();
      await expect(queued).rejects.toMatchObject({ name: "AbortError" });
      expect(run).not.toHaveBeenCalled();
      expect(release).toHaveBeenCalledOnce();
      // The active turn keeps its own fence; the aborted waiter never touched the row.
      expect(read(target)?.restartRecoveryRuns).toEqual([
        { runId: activeRunId, lifecycleGeneration },
      ]);
    } finally {
      clear();
    }
  });

  it("fails with the original error once its own timeout passes", async () => {
    readRowInProcess();
    vi.useFakeTimers();
    const target = { ...createTarget(), timeoutMs: 5_000 };
    const lifecycleGeneration = getAgentEventLifecycleGeneration();
    await write(target, activeFence(lifecycleGeneration));
    const clear = startActiveTurn(lifecycleGeneration);
    const run = vi.fn();
    try {
      const queued = execute(target, {
        lifecycleGeneration,
        opts: { runId: "queued-turn" } as AgentCommandOpts,
        run,
      });
      void queued.catch(() => {});
      await vi.advanceTimersByTimeAsync(4_000);
      expect(run).not.toHaveBeenCalled();
      await vi.advanceTimersByTimeAsync(1_000);
      await expect(queued).rejects.toMatchObject({ code: "SESSION_WORK_START_CHANGED" });
      expect(run).not.toHaveBeenCalled();
    } finally {
      clear();
    }
  });

  it("stops after the settle grace when the finished turn keeps its fence", async () => {
    readRowInProcess();
    vi.useFakeTimers();
    const target = { ...createTarget(), timeoutMs: 600_000 };
    const lifecycleGeneration = getAgentEventLifecycleGeneration();
    await write(target, activeFence(lifecycleGeneration));
    const clear = startActiveTurn(lifecycleGeneration);
    const run = vi.fn();
    try {
      const queued = execute(target, {
        lifecycleGeneration,
        opts: { runId: "queued-turn" } as AgentCommandOpts,
        run,
      });
      void queued.catch(() => {});
      await vi.advanceTimersByTimeAsync(10);
      emitAgentEvent({ runId: activeRunId, stream: "lifecycle", data: { phase: "end" } });
      clear();
      await vi.advanceTimersByTimeAsync(LIVE_RUN_FENCE_SETTLE_GRACE_MS);
      // The renewed claim uses the real store writer.
      vi.useRealTimers();
      await expect(queued).rejects.toMatchObject({ code: "SESSION_WORK_START_CHANGED" });
      expect(run).not.toHaveBeenCalled();
    } finally {
      clear();
    }
  });

  it("does not run after the Gateway lifecycle rotates during the wait", async () => {
    const target = { ...createTarget(), timeoutMs: 60_000 };
    const lifecycleGeneration = getAgentEventLifecycleGeneration();
    await write(target, activeFence(lifecycleGeneration));
    const clear = startActiveTurn(lifecycleGeneration);
    const run = vi.fn();
    try {
      const queued = execute(target, {
        lifecycleGeneration,
        opts: { runId: "queued-turn" } as AgentCommandOpts,
        run,
      });
      void queued.catch(() => {});
      await settleTicks();
      rotateAgentEventLifecycleGeneration();
      await finishActiveTurn(target, clear, "error");
      await expect(queued).rejects.toThrow();
      expect(run).not.toHaveBeenCalled();
    } finally {
      clear();
    }
  });
});
