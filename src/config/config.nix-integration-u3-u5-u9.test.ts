import path from "node:path";
import { describe, expect, it } from "vitest";
import { resolveConfigPath } from "./config.js";
import { withTempHome } from "./test-helpers.js";

describe("Nix integration config selection", () => {
  it("defaults CONFIG_PATH to OPENCLAW_HOME/.openclaw/openclaw.json", () => {
    const customHome = path.join(path.sep, "custom", "home");
    expect(resolveConfigPath({ OPENCLAW_HOME: customHome })).toBe(
      path.join(path.resolve(customHome), ".openclaw", "openclaw.json"),
    );
  });

  it("expands ~ in OPENCLAW_CONFIG_PATH override", async () => {
    await withTempHome(async (home) => {
      expect(
        resolveConfigPath(
          { OPENCLAW_HOME: home, OPENCLAW_CONFIG_PATH: "~/.openclaw/custom.json" },
          undefined,
          () => home,
        ),
      ).toBe(path.join(home, ".openclaw", "custom.json"));
    });
  });

  it("uses STATE_DIR when only state dir is overridden", () => {
    expect(
      resolveConfigPath(
        { OPENCLAW_STATE_DIR: "/custom/state", OPENCLAW_TEST_FAST: "1" },
        undefined,
        () => path.join(path.sep, "tmp", "openclaw-config-home"),
      ),
    ).toBe(path.join(path.resolve("/custom/state"), "openclaw.json"));
  });
});
