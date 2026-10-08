// Lets ingress work queue behind a turn that is still executing on the same session.
import { resolveTimerTimeoutMs } from "@openclaw/normalization-core/number-coercion";
import type { InternalSessionEntry } from "../config/sessions.js";
import { isMainRestartRecoveryCandidate } from "../config/sessions/restart-recovery-state.js";
import { loadSessionEntry } from "../config/sessions/session-accessor.js";
import {
  isAgentEventLifecycleGenerationCurrent,
  onAgentEventForRun,
} from "../infra/agent-events.js";
import { getAgentRunContext, hasLiveAgentRunContext } from "../infra/agent-run-registry.js";
import { sessionChanges } from "../sessions/session-row-changes.js";
import { resolveActiveEmbeddedRunOwnerByRunId } from "./embedded-agent-runner/runs.js";

/** Time a finished turn's terminal write gets to retire its fence before admission fails again. */
export const LIVE_RUN_FENCE_SETTLE_GRACE_MS = 15_000;

export type LiveRunFenceTarget = {
  agentId?: string;
  sessionId: string;
  sessionKey: string;
  storePath: string;
};

function readEntry(target: LiveRunFenceTarget): InternalSessionEntry | undefined {
  return loadSessionEntry({
    agentId: target.agentId,
    sessionKey: target.sessionKey,
    storePath: target.storePath,
  });
}

/**
 * A turn holds its runtime handle while the model runs and its run context while tools run;
 * either one proves this exact run is still executing for this session.
 */
function isRunExecutingForSession(runId: string, target: LiveRunFenceTarget): boolean {
  const owner = resolveActiveEmbeddedRunOwnerByRunId(runId);
  if (owner) {
    return owner.sessionId === target.sessionId;
  }
  return (
    hasLiveAgentRunContext(runId) && getAgentRunContext(runId)?.sessionKey === target.sessionKey
  );
}

/**
 * Run ids whose fences are the only reason admission was rejected: every fence belongs to an
 * execution of this session that is still running in the current lifecycle. A live turn may
 * hold its own delivery claim. Recovery or delivery debt of any other run, other generations,
 * runs this waiter already saw end, and retained fences without a live execution (for example
 * after `sessions_yield`) return undefined, so those rows keep failing admission unchanged.
 */
export function resolveLiveRunFenceIds(params: {
  endedRunIds: ReadonlySet<string>;
  lifecycleGeneration: string;
  runId?: string;
  target: LiveRunFenceTarget;
}): string[] | undefined {
  const entry = readEntry(params.target);
  const runs = entry?.restartRecoveryRuns;
  if (
    !entry ||
    !runs?.length ||
    entry.sessionId !== params.target.sessionId ||
    entry.abortedLastRun === true ||
    entry.mainRestartRecovery !== undefined ||
    entry.pendingFinalDelivery !== undefined ||
    (entry.restartRecoveryDeliveryRunId !== undefined &&
      !runs.some((run) => run.runId === entry.restartRecoveryDeliveryRunId)) ||
    !isMainRestartRecoveryCandidate(entry, params.target.sessionKey) ||
    !isAgentEventLifecycleGenerationCurrent(params.lifecycleGeneration)
  ) {
    return undefined;
  }
  const live = runs.every(
    (run) =>
      run.runId !== params.runId &&
      !params.endedRunIds.has(run.runId) &&
      run.lifecycleGeneration === params.lifecycleGeneration &&
      isRunExecutingForSession(run.runId, params.target),
  );
  return live ? runs.map((run) => run.runId) : undefined;
}

/**
 * Settles once none of these fences remain on the durable row, the row changes owner, the
 * lifecycle rotates, a grace period after the fenced runs end, the waiter aborts, or at the
 * deadline. Settling grants nothing: the caller prepares and claims again under the unchanged
 * admission rules.
 */
export function waitForLiveRunFences(params: {
  deadline: number;
  /** Records fenced runs whose terminal event arrived, so a renewed claim never waits on them. */
  endedRunIds: Set<string>;
  lifecycleGeneration: string;
  runIds: readonly string[];
  signal?: AbortSignal;
  target: LiveRunFenceTarget;
}): Promise<void> {
  return new Promise((resolve) => {
    const cleanups: Array<() => void> = [];
    let settled = false;
    let graceTimer: ReturnType<typeof setTimeout> | undefined;
    const finish = () => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(deadlineTimer);
      clearTimeout(graceTimer);
      for (const cleanup of cleanups) {
        cleanup();
      }
      resolve();
    };
    const check = () => {
      if (settled) {
        return;
      }
      const entry = readEntry(params.target);
      if (
        !isAgentEventLifecycleGenerationCurrent(params.lifecycleGeneration) ||
        entry?.sessionId !== params.target.sessionId ||
        !entry.restartRecoveryRuns?.some((run) => params.runIds.includes(run.runId))
      ) {
        finish();
      }
    };
    const deadlineTimer = setTimeout(
      finish,
      resolveTimerTimeoutMs(params.deadline - Date.now(), 0, 0),
    );
    deadlineTimer.unref?.();
    if (params.signal) {
      const signal = params.signal;
      signal.addEventListener("abort", finish, { once: true });
      cleanups.push(() => signal.removeEventListener("abort", finish));
      if (signal.aborted) {
        finish();
        return;
      }
    }
    cleanups.push(
      sessionChanges.subscribe((change) => {
        if (!("sessionKey" in change) || change.sessionKey === params.target.sessionKey) {
          check();
        }
      }),
    );
    for (const runId of params.runIds) {
      cleanups.push(
        onAgentEventForRun(runId, (event) => {
          const phase = event.data?.phase;
          if (event.stream !== "lifecycle" || (phase !== "end" && phase !== "error")) {
            return;
          }
          params.endedRunIds.add(runId);
          check();
          // The terminal write normally retires the fence right after this event. A fence
          // the run keeps (for example a yield) must not hold the waiter until its deadline.
          graceTimer ??= setTimeout(finish, LIVE_RUN_FENCE_SETTLE_GRACE_MS);
          graceTimer.unref?.();
        }),
      );
    }
    check();
  });
}
