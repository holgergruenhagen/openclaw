import path from "node:path";
import { afterAll, afterEach, expect, it, vi } from "vitest";
import {
  awaitGateBeforeSettlement,
  createDeferred,
  withinTest,
} from "../../test/helpers/promise.js";
import { admitReplyTurn } from "../auto-reply/reply/reply-turn-admission.js";
import { loadSessionEntry, replaceSessionEntry } from "../config/sessions/session-accessor.js";
import { getAgentEventLifecycleGeneration } from "../infra/agent-events.js";
import {
  beginSessionWorkAdmission,
  getSessionWorkAdmissionRelease,
  type SessionWorkAdmissionLease,
} from "../sessions/session-lifecycle-admission.js";
import { useSessionStoreTempDirs } from "../test-utils/session-state-cleanup.js";
import { runWithAgentCommandRecoveryOwner } from "./agent-command-recovery-owner.js";
import * as recoveryStore from "./main-session-recovery/main-session-recovery-store.js";

const sessionDirs = useSessionStoreTempDirs(afterAll, "openclaw-command-reply-admission-");
const sessionKey = "agent:main:main";

afterEach(() => {
  vi.restoreAllMocks();
});

async function busyReply() {
  const target = {
    sessionAgentId: "main",
    isNewSession: false,
    sessionId: "session-1",
    sessionKey,
    storePath: path.join(sessionDirs.make(), "sessions.json"),
  };
  const entry = { sessionId: target.sessionId, updatedAt: Date.now() };
  await replaceSessionEntry(target, entry);
  const reply = await admitReplyTurn({
    ...target,
    agentId: target.sessionAgentId,
    expectedSessionId: target.sessionId,
    kind: "visible",
    resetTriggered: false,
  });
  if (reply.status !== "owned") {
    throw new Error("Fixture requires an admitted reply");
  }
  const scope = { scope: target.storePath, identities: [sessionKey, target.sessionId] };
  const replyReleased = getSessionWorkAdmissionRelease(scope);
  if (!replyReleased) {
    throw new Error("Reply admission must retain its lifecycle lease");
  }
  const lifecycleGeneration = getAgentEventLifecycleGeneration();
  const fence = [{ runId: "busy-reply", lifecycleGeneration }];
  await replaceSessionEntry(target, {
    ...entry,
    abortedLastRun: false,
    restartRecoveryRuns: fence,
    restartRecoveryDeliveryRunId: "busy-reply",
  });
  const controller = new AbortController();
  const commands: Promise<unknown>[] = [];
  const gateways: SessionWorkAdmissionLease[] = [];
  type Command = Parameters<typeof runWithAgentCommandRecoveryOwner<typeof target, string>>[0];
  return {
    target,
    controller,
    fence,
    read: () => loadSessionEntry(target),
    finish: async (retainFence = false) => {
      if (!retainFence) {
        await replaceSessionEntry(target, { ...entry, status: "done" });
      }
      reply.operation.complete();
      await replyReleased;
    },
    gateway: async () => {
      const admission = await beginSessionWorkAdmission({ ...scope, assertAllowed: () => {} });
      gateways.push(admission);
      return admission;
    },
    command: (runId: string, overrides: Partial<Command> = {}) => {
      const command = runWithAgentCommandRecoveryOwner({
        lifecycleGeneration,
        mode: "claim",
        opts: { message: runId, runId, abortSignal: controller.signal },
        prepare: async () => target,
        run: async () => runId,
        ...overrides,
      });
      commands.push(command);
      void command.catch(() => {});
      return command;
    },
    cleanup: async () => {
      controller.abort();
      reply.operation.complete();
      gateways.forEach((gateway) => gateway.release());
      await Promise.allSettled(commands);
      await replyReleased;
    },
  };
}

function pauseRejectedClaim(runId: string, release?: Promise<void>) {
  const rejected = createDeferred();
  const claim = recoveryStore.claimMainSessionRecoveryOwner;
  vi.spyOn(recoveryStore, "claimMainSessionRecoveryOwner").mockImplementation(async (params) => {
    const result = await claim(params);
    if (params.runId === runId && result.kind === "invalidated") {
      rejected.resolve();
      await release;
    }
    return result;
  });
  return rejected.promise;
}

it("keeps two accepted commands in FIFO order behind a real reply admission", async ({
  signal,
}) => {
  const fixture = await busyReply();
  const refresh = createDeferred();
  const refreshing = createDeferred();
  const secondPrepared = createDeferred();
  const rejected = pauseRejectedClaim("first");
  const order: string[] = [];
  let preparations = 0;
  try {
    // Both accepted RPCs hold outer admissions; the second command depends on the first.
    const firstGateway = await fixture.gateway();
    const secondGateway = await fixture.gateway();
    const first = firstGateway.run(() =>
      fixture.command("first", {
        prepare: async () => {
          if (++preparations === 2) {
            refreshing.resolve();
            await refresh.promise;
          }
          return fixture.target;
        },
        run: async () => {
          order.push("first");
          return "first";
        },
      }),
    );
    await withinTest(
      awaitGateBeforeSettlement(rejected, first, "Command skipped the busy claim"),
      signal,
    );
    const second = secondGateway.run(() =>
      fixture.command("second", {
        prepare: async () => {
          secondPrepared.resolve();
          return fixture.target;
        },
        run: async () => {
          order.push("second");
          return "second";
        },
      }),
    );
    await withinTest(secondPrepared.promise, signal);
    expect(order).toEqual([]);
    await fixture.finish();
    await withinTest(
      awaitGateBeforeSettlement(
        refreshing.promise,
        first,
        "Command did not refresh after the reply",
      ),
      signal,
    );
    expect(order).toEqual([]);
    refresh.resolve();
    await expect(withinTest(Promise.all([first, second]), signal)).resolves.toEqual([
      "first",
      "second",
    ]);
    expect(order).toEqual(["first", "second"]);
  } finally {
    refresh.resolve();
    await fixture.cleanup();
  }
});

it.each(["cancelled", "retained fence"] as const)(
  "does not execute the waiting command after %s",
  async (outcome, { signal }) => {
    const fixture = await busyReply();
    const rejected = pauseRejectedClaim("waiting");
    const run = vi.fn(async () => "ran");
    try {
      const command = fixture.command("waiting", { run });
      await withinTest(
        awaitGateBeforeSettlement(rejected, command, "Command skipped the busy claim"),
        signal,
      );
      if (outcome === "cancelled") {
        fixture.controller.abort();
      } else {
        await fixture.finish(true);
      }
      await expect(withinTest(command, signal)).rejects.toMatchObject(
        outcome === "cancelled" ? { name: "AbortError" } : { code: "SESSION_WORK_START_CHANGED" },
      );
      expect(run).not.toHaveBeenCalled();
      expect(fixture.read()?.restartRecoveryRuns).toEqual(fixture.fence);
    } finally {
      await fixture.cleanup();
    }
  },
);

it("keeps the reply release captured before its durable claim returns", async ({ signal }) => {
  const fixture = await busyReply();
  const returnClaim = createDeferred();
  const rejected = pauseRejectedClaim("racing", returnClaim.promise);
  const run = vi.fn(async () => "ran");
  try {
    const command = fixture.command("racing", { run });
    await withinTest(
      awaitGateBeforeSettlement(rejected, command, "Command skipped the busy claim"),
      signal,
    );
    await fixture.finish();
    returnClaim.resolve();
    await expect(withinTest(command, signal)).resolves.toBe("ran");
    expect(run).toHaveBeenCalledOnce();
  } finally {
    returnClaim.resolve();
    await fixture.cleanup();
  }
});
