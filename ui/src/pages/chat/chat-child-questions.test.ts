import { describe, expect, it } from "vitest";
import type { GatewaySessionRow } from "../../api/types.ts";
import type { QuestionPrompt } from "../../app/question-prompt.ts";
import { projectChildSessionQuestions } from "./chat-child-questions.ts";

const host = { assistantAgentId: "main" };
const parentKey = "agent:main:dashboard:parent";

function row(key: string, fields: Partial<GatewaySessionRow> = {}): GatewaySessionRow {
  return { key, kind: "direct", ...fields };
}

function prompt(id: string, sessionKey: string, fields: Partial<QuestionPrompt> = {}) {
  return {
    id,
    questions: [{ questionId: "choice", header: "Choice", question: "Which one?", options: [] }],
    agentId: "main",
    sessionKey,
    createdAtMs: 1_000,
    expiresAtMs: 61_000,
    status: "pending",
    answeredElsewhere: false,
    localResolutionConfirmed: false,
    locallyExpired: false,
    submitting: false,
    error: null,
    drafts: new Map(),
    revision: 1,
    ...fields,
  } satisfies QuestionPrompt;
}

const visibleChild = row("agent:main:dashboard:child", {
  label: "Release checks",
  parentSessionKey: parentKey,
  spawnDepth: 1,
});

describe("projectChildSessionQuestions", () => {
  it("docks a pending question from a direct child with the child's name", () => {
    const own = prompt("own", parentKey);
    const asked = prompt("child", visibleChild.key);
    const result = projectChildSessionQuestions({
      host,
      parentKey,
      children: [row(parentKey), visibleChild],
      prompts: [own, asked],
    });
    expect(result.prompts).toEqual([asked]);
    expect([...result.sources]).toEqual([["child", "Release checks"]]);
  });

  it("leaves answered, unrelated, archived, and grandchild questions out", () => {
    const grandchild = row("agent:main:dashboard:grandchild", {
      parentSessionKey: visibleChild.key,
      spawnDepth: 2,
    });
    const archived = row("agent:main:dashboard:archived", {
      parentSessionKey: parentKey,
      spawnDepth: 1,
      archived: true,
    });
    const result = projectChildSessionQuestions({
      host,
      parentKey,
      children: [visibleChild, grandchild, archived],
      prompts: [
        prompt("answered", visibleChild.key, { status: "answered" }),
        prompt("other-agent", visibleChild.key, { agentId: "ops" }),
        prompt("unrelated", "agent:main:dashboard:unrelated"),
        prompt("grandchild", grandchild.key),
        prompt("archived", archived.key),
      ],
    });
    expect(result.prompts).toEqual([]);
  });

  it("does not treat operator roots linked to Home as children", () => {
    const home = "agent:main:main";
    const operatorRoot = row("agent:main:dashboard:root", {
      parentSessionKey: home,
      createdVia: "operator",
      spawnDepth: 0,
    });
    const spawnedFromHome = row("agent:main:dashboard:spawned", {
      parentSessionKey: home,
      createdVia: "operator",
      spawnDepth: 1,
    });
    const result = projectChildSessionQuestions({
      host,
      parentKey: home,
      children: [operatorRoot, spawnedFromHome],
      prompts: [prompt("root", operatorRoot.key), prompt("spawned", spawnedFromHome.key)],
    });
    expect(result.prompts.map(({ id }) => id)).toEqual(["spawned"]);
  });
});
