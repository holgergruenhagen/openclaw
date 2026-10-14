import type { GatewaySessionRow } from "../../api/types.ts";
import type { QuestionPrompt } from "../../app/question-prompt.ts";
import { resolveSidebarSessionParentKey } from "../../components/app-sidebar-session-parent.ts";
import { resolveSessionDisplayName } from "../../lib/session-display.ts";
import {
  areUiSessionKeysEquivalent,
  uiConversationMatches,
  type UiSessionDefaultsHost,
} from "../../lib/sessions/session-key.ts";

/**
 * Pending questions asked by the conversation's direct child sessions. A visible
 * spawned session is an ordinary session that can call `ask_user`; without this
 * its question docks only in the child while the parent waits for it to finish.
 * Answered questions stay in the child's own transcript, so only pending ones
 * dock here, and answers still resolve through the child question's id.
 */
export function projectChildSessionQuestions(params: {
  host: UiSessionDefaultsHost;
  parentKey: string | undefined;
  children: readonly GatewaySessionRow[] | undefined;
  prompts: readonly QuestionPrompt[];
}): { prompts: QuestionPrompt[]; sources: Map<string, string> } {
  const prompts: QuestionPrompt[] = [];
  const sources = new Map<string, string>();
  const { parentKey } = params;
  if (!parentKey) {
    return { prompts, sources };
  }
  // The roster can hold seeded rows and the parent itself. The implicit Home
  // link of operator-created roots does not make them children of Home.
  const children = (params.children ?? []).filter(
    (row) =>
      !row.archived &&
      !areUiSessionKeysEquivalent(row.key, parentKey) &&
      areUiSessionKeysEquivalent(
        resolveSidebarSessionParentKey(row, new Set([parentKey])),
        parentKey,
      ),
  );
  for (const prompt of params.prompts) {
    if (prompt.status !== "pending") {
      continue;
    }
    const child = children.find((row) =>
      uiConversationMatches(params.host, row.key, prompt.sessionKey, prompt.agentId),
    );
    if (child) {
      prompts.push(prompt);
      sources.set(prompt.id, resolveSessionDisplayName(child.key, child));
    }
  }
  return { prompts, sources };
}
