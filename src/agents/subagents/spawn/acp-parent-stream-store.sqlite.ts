// ACP parent-stream diagnostics live with their child session in the per-agent database.
import { isRecord } from "@openclaw/normalization-core/record-coerce";
import { cloneEnvWithPlatformSemantics } from "../../../config/config-env-vars.js";
import { resolveStateDir } from "../../../config/state-dir.js";
import { SqliteWorkerError } from "../../../infra/sqlite-worker-contract.js";
import {
  createSqliteWorkerOperationAdmission,
  type SqliteWorkerOperationAdmission,
} from "../../../infra/sqlite-worker-operation-admission.js";
import type { RetainedWorkerTransactionAdmission } from "../../../infra/sqlite-worker-operation-settlement.js";
import { createSubsystemLogger } from "../../../logging/subsystem.js";
import type { OpenClawAgentDatabaseOptions } from "../../../state/openclaw-agent-db-contract.js";
import type { AgentDatabaseRequestExecutionSource } from "../../../state/openclaw-agent-execution-contract.js";
import { captureOpenClawAgentDatabaseExecution } from "../../../state/openclaw-agent-execution.js";
import { runOpenClawAgentWorkerWrite } from "../../../state/openclaw-agent-write-admission.js";

const log = createSubsystemLogger("agents/acp-parent-stream");

export type AcpParentStreamEvent = Record<string, unknown>;

/** Records one ordered batch through the child database's canonical writer. */
export async function recordAcpParentStreamEvents(
  options: OpenClawAgentDatabaseOptions & {
    sessionId: string;
    runId: string;
    events: Array<{ event: AcpParentStreamEvent; createdAt: number }>;
  },
): Promise<void> {
  if (options.events.length === 0) {
    return;
  }
  const prepared = options.events.flatMap((entry) => {
    try {
      const eventJson = JSON.stringify(entry.event);
      if (eventJson !== undefined) {
        return [{ eventJson, createdAt: entry.createdAt }];
      }
    } catch {
      // One malformed diagnostic must not poison later valid events or retries.
    }
    return [];
  });
  if (prepared.length === 0) {
    return;
  }
  const env = cloneEnvWithPlatformSemantics(options.env ?? process.env);
  env.OPENCLAW_STATE_DIR = resolveStateDir(env);
  const databaseOptions = { ...options, env };
  const input = { sessionId: options.sessionId, runId: options.runId, events: prepared };
  const execution = captureOpenClawAgentDatabaseExecution(databaseOptions);
  let transaction:
    | { admission: SqliteWorkerOperationAdmission; retained: RetainedWorkerTransactionAdmission }
    | undefined;
  const source: AgentDatabaseRequestExecutionSource = {
    assertCurrent: () => execution.assertCurrent(),
    createAdmission(binding) {
      return (retained) => {
        const admission = createSqliteWorkerOperationAdmission((request, grant) => {
          binding.authorize(request);
          execution.assertCurrent();
          if (request.stage === "transaction") {
            transaction = { admission, retained };
          }
          if (!grant()) {
            throw new Error("ACP parent stream database authority expired");
          }
        }, binding.attachment);
        return { nativeLocations: binding.nativeLocations, admission };
      };
    },
  };
  const outcome = await Promise.resolve()
    .then(() =>
      runOpenClawAgentWorkerWrite(databaseOptions, async () => {
        await execution.prepare(source);
        const written = await execution.runExisting(source, async (worker) => {
          const result = await worker.execute({ type: "acp.parentStream.record", input }).then(
            () => ({ ok: true as const }),
            (error: unknown) => ({ ok: false as const, error }),
          );
          if (transaction) {
            await transaction.retained.settled;
          }
          const receipt = transaction?.admission.committed?.facts;
          if (result.ok || (isRecord(receipt) && receipt.kind === "acp-parent-stream-recorded")) {
            return true;
          }
          if (transaction?.admission.settlement?.kind === "unknown") {
            const error = new SqliteWorkerError(
              "ACP parent stream write outcome is unknown; this batch cannot be replayed",
              "outcome-unknown",
            );
            error.cause = result.error;
            throw error;
          }
          throw result.error;
        });
        if (!written) {
          throw new Error("ACP parent stream database disappeared before append");
        }
      }),
    )
    .then(
      () => ({ ok: true as const }),
      (error: unknown) => ({ ok: false as const, error }),
    );
  try {
    await execution.release();
  } catch (error) {
    if (!outcome.ok) {
      throw new AggregateError(
        [outcome.error, error],
        "ACP parent stream write and cleanup failed",
        { cause: error },
      );
    }
    // A confirmed append must never become a replayable failure after cleanup.
    log.warn("ACP parent stream diagnostics committed before writer cleanup failed", {
      runId: input.runId,
      error: String(error),
    });
  }
  if (!outcome.ok) {
    throw outcome.error;
  }
}
