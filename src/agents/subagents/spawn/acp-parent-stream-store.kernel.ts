import {
  executeSqliteQuerySync,
  executeSqliteQueryTakeFirstSync,
  getNodeSqliteKysely,
} from "../../../infra/kysely-sync.js";
import { coerceRequiredSqliteNumber as sqliteNumber } from "../../../infra/sqlite-number.js";
import type { OpenClawAgentDatabase } from "../../../state/openclaw-agent-db-contract.js";
import type { DB as OpenClawAgentKyselyDatabase } from "../../../state/openclaw-agent-db.generated.js";

type AcpParentStreamDatabase = Pick<OpenClawAgentKyselyDatabase, "acp_parent_stream_events">;

/** Sequence allocation and insertion share the caller's admitted worker transaction. */
export function recordAcpParentStreamEventsInDatabase(
  database: OpenClawAgentDatabase,
  input: {
    sessionId: string;
    runId: string;
    events: Array<{ eventJson: string; createdAt: number }>;
  },
): void {
  const db = getNodeSqliteKysely<AcpParentStreamDatabase>(database.db);
  const row = executeSqliteQueryTakeFirstSync(
    database.db,
    db
      .selectFrom("acp_parent_stream_events")
      .select((eb) => eb.fn.max<number | bigint>("seq").as("max_seq"))
      .where("session_id", "=", input.sessionId)
      .where("run_id", "=", input.runId),
  );
  const firstSeq =
    row?.max_seq === null || row?.max_seq === undefined ? 0 : sqliteNumber(row.max_seq) + 1;
  executeSqliteQuerySync(
    database.db,
    db.insertInto("acp_parent_stream_events").values(
      input.events.map((entry, index) => ({
        session_id: input.sessionId,
        run_id: input.runId,
        seq: firstSeq + index,
        event_json: entry.eventJson,
        created_at: entry.createdAt,
      })),
    ),
  );
}
