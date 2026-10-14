// Tests queued chat input behind an embedded run that holds the session lane without a
// reply operation, as inter-session (sessions_send) turns do.
import { setTimeout as sleep } from "node:timers/promises";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createDeferred } from "../../../test/helpers/promise.js";
import {
  clearRuntimeConfigSnapshot,
  setRuntimeConfigSnapshot,
} from "../../config/runtime-snapshot.js";
import { abortQueuedChatTurnById } from "../../gateway/chat-queued-turns.js";
import { createDirectChatContext } from "../../gateway/server-chat.agent-events.test-helpers.js";
import { createChatSendTurnAdoptionLifecycle } from "../../gateway/server-methods/chat-send-turn-adoption.js";
import type { TemplateContext } from "../templating.js";
import type * as AgentRunnerExecution from "./agent-runner-execution.js";
import { runReplyAgent } from "./agent-runner-run.js";
import { createTestFollowupRun } from "./agent-runner.test-fixtures.js";
import { getFollowupQueueDepth } from "./queue.js";
import { clearFollowupDrainCallback } from "./queue/drain.js";
import { clearFollowupQueue } from "./queue/state.js";
import { testing as replyRunTesting } from "./reply-run-registry.test-support.js";
import { createMockTypingController } from "./test-helpers.js";

const mocks = vi.hoisted(() => ({ executeAgentTurn: vi.fn() }));

vi.mock("./agent-runner-memory.js", () => ({
  runSessionCompactionIfNeeded: async () => undefined,
  runMemoryFlushIfNeeded: async () => ({ sessionEntry: undefined, outcome: "skipped" }),
}));

vi.mock("./agent-runner-execution.js", async () => ({
  ...(await vi.importActual<typeof AgentRunnerExecution>("./agent-runner-execution.js")),
  executeAgentTurn: (...args: unknown[]) => mocks.executeAgentTurn(...args),
}));

const sessionKey = "agent:main:inter-session-queue";
const sessionId = "inter-session-session";

function fixture() {
  const cfg = {};
  setRuntimeConfigSnapshot(cfg);
  const context = createDirectChatContext({
    chatQueuedTurns: new Map(),
    getRuntimeConfig: () => cfg,
  });
  // The inter-session turn owns the lane through its embedded run only.
  const embeddedRun = createDeferred();
  let embeddedRunActive = true;
  void embeddedRun.promise.then(() => {
    embeddedRunActive = false;
  });

  const send = async (runId: string) => {
    const controller = new AbortController();
    const adoption = createChatSendTurnAdoptionLifecycle({
      accountId: undefined,
      context,
      chatQueuedTurns: context.chatQueuedTurns,
      runId,
      controller,
      sessionBinding: { sessionKey, sessionId, agentId: "main", lifecycleGeneration: "test" },
      sessionKey,
      agentId: "main",
      originatingChannel: "webchat",
      session: {
        agentId: "main",
        backingSessionId: sessionId,
        cfg,
        clientRunId: runId,
        sessionKey,
        sessionLoadOptions: { agentId: "main" },
      },
      hasCronCreatorAuthority: false,
      releaseSourceWorkAdmission: vi.fn(),
      retainWorkAdmission: () => vi.fn(),
    });
    const followupRun = createTestFollowupRun({
      sessionId,
      sessionKey,
      messageProvider: "webchat",
    });
    followupRun.prompt = runId;
    followupRun.originatingChannel = "webchat";
    followupRun.abortSignal = controller.signal;
    followupRun.turnAdoptionLifecycle = adoption.lifecycle;
    followupRun.onQueueDisposition = adoption.onQueueDisposition;
    followupRun.queuedFollowupReplyDisposition = {
      kind: "deliver",
      deliver: adoption.onQueuedFollowupReplyBatch,
    };
    await runReplyAgent({
      commandBody: runId,
      followupRun,
      queueKey: sessionKey,
      resolvedQueue: { mode: "followup", debounceMs: 0 },
      shouldSteer: false,
      shouldFollowup: true,
      isActive: true,
      isRunActive: () => embeddedRunActive,
      waitForRunEnd: () => embeddedRun.promise,
      typing: createMockTypingController(),
      sessionCtx: {
        Provider: "webchat",
        OriginatingChannel: "webchat",
        ChatType: "direct",
        MessageSid: runId,
      } as unknown as TemplateContext,
      defaultModel: "anthropic/claude",
      resolvedVerboseLevel: "off",
      isNewSession: false,
      blockStreamingEnabled: false,
      resolvedBlockStreamingBreak: "message_end",
      shouldInjectGroupIntro: false,
      typingMode: "instant",
    });
    expect(context.chatQueuedTurns.has(runId)).toBe(true);
  };

  // Mirrors chat.abort with discardPendingInput: withdraw only while adoption has not started.
  const withdraw = (runId: string) => {
    const release = context.chatQueuedTurns.get(runId)?.holdPendingInputWithdrawal?.();
    if (!release) {
      return false;
    }
    try {
      return abortQueuedChatTurnById(context.chatQueuedTurns, {
        runId,
        sessionKey,
        stopReason: "rpc",
      }).aborted;
    } finally {
      release();
    }
  };
  const terminals = () =>
    vi
      .mocked(context.broadcast)
      .mock.calls.flatMap(([event, payload]) => (event === "chat" ? [payload] : []));
  return { send, withdraw, terminals, endEmbeddedRun: embeddedRun.resolve };
}

describe("runReplyAgent queued chat input behind an inter-session run", () => {
  beforeEach(() => {
    replyRunTesting.resetReplyRunRegistry();
    mocks.executeAgentTurn.mockReset().mockImplementation(async () => ({
      runId: "queued-execution",
      outcome: { kind: "rejected", payload: { text: "queued reply" } },
    }));
  });

  afterEach(() => {
    clearFollowupQueue(sessionKey);
    clearFollowupDrainCallback(sessionKey);
    replyRunTesting.resetReplyRunRegistry();
    clearRuntimeConfigSnapshot();
  });

  it("keeps queued input withdrawable and runs only the retained input after the run ends", async () => {
    const { send, withdraw, terminals, endEmbeddedRun } = fixture();
    await send("withdrawn-input");
    await send("retained-input");
    // Gives an eagerly scheduled drain time to reach followup admission.
    await sleep(20);

    expect(mocks.executeAgentTurn).not.toHaveBeenCalled();
    expect(withdraw("withdrawn-input")).toBe(true);
    expect(terminals()).toEqual([
      expect.objectContaining({ runId: "withdrawn-input", state: "aborted" }),
    ]);

    endEmbeddedRun();
    await vi.waitFor(() => expect(getFollowupQueueDepth(sessionKey)).toBe(0));
    await vi.waitFor(() => expect(mocks.executeAgentTurn).toHaveBeenCalledOnce());
    expect(mocks.executeAgentTurn.mock.calls[0]?.[0]).toMatchObject({
      followupRun: expect.objectContaining({ prompt: "retained-input" }),
    });
  });
});
