import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../../../", import.meta.url));
const destination = path.join(root, "apps/linux/src-tauri/target/desktop-runtime");

export function resourceBytes(platform, executable) {
  // linuxdeploy rewrites ELF resources even without executable permissions.
  // This fixed envelope is stripped by bundled_runtime.rs; hashes cover raw bytes.
  return platform === "linux"
    ? Buffer.concat([Buffer.from("OPENCLAW-BUN-RUNTIME-V1\n"), executable])
    : executable;
}

export function runtimeTarget(triple) {
  if (triple.includes("-windows-") || triple.endsWith("-unknown-freebsd")) {
    return null; // Preserve Node where no bundled fork is published.
  }
  const platform = triple.endsWith("-apple-darwin")
    ? "darwin"
    : triple.endsWith("-unknown-linux-gnu")
      ? "linux"
      : null;
  const arch = triple.startsWith("aarch64-") ? "arm64" : triple.startsWith("x86_64-") ? "x64" : null;
  if (!platform || !arch) {
    throw new Error(`Unsupported embedded runtime target: ${triple}`);
  }
  return { platform, arch };
}

function run(script, args, env = process.env) {
  execFileSync(script, args, { cwd: root, env, stdio: "inherit" });
}

export function stageRuntime(triple) {
  const target = runtimeTarget(triple);
  fs.mkdirSync(path.dirname(destination), { recursive: true });
  const work = fs.mkdtempSync(`${destination}-`);
  try {
    if (target) {
      const pin = JSON.parse(fs.readFileSync(path.join(root, "scripts/lib/openclaw-bun.json"), "utf8"));
      run(path.join(root, "scripts/stage-openclaw-bun.sh"), [work, target.platform, target.arch]);
      const files = ["bin/bun"];
      if (target.platform === "darwin") {
        run(path.join(root, "scripts/build-mac-sqlite.sh"), [
          target.arch === "x64" ? "x86_64" : "arm64",
          work,
        ], { ...process.env, OPENCLAW_MACOS_DEPLOYMENT_TARGET: "13.0" });
        files.push("lib/libsqlite3.dylib");
        const identity = process.env.APPLE_SIGNING_IDENTITY;
        if (identity) {
          if (!identity.startsWith("Developer ID Application:")) {
            throw new Error("The embedded runtime requires a Developer ID Application signing identity.");
          }
          for (const file of files) {
            const entitlements = file === "bin/bun"
              ? ["--entitlements", path.join(root, "apps/linux/src-tauri/bun.entitlements.plist")]
              : [];
            run("/usr/bin/codesign", ["--force", "--options", "runtime", "--timestamp", ...entitlements,
              "--sign", identity, path.join(work, file)]);
            run("/usr/bin/codesign", ["--verify", "--strict", path.join(work, file)]);
          }
        }
      }
      // Hash executable bytes after signing, before the Linux resource envelope.
      // macOS signatures and the raw hashes in the shared pin remain unchanged.
      const hashes = Object.fromEntries(files.map((file) => {
        const executable = fs.readFileSync(path.join(work, file));
        if (target.platform === "linux") {
          fs.writeFileSync(path.join(work, file), resourceBytes(target.platform, executable));
        }
        fs.chmodSync(path.join(work, file), 0o644);
        return [file, createHash("sha256").update(executable).digest("hex")];
      }));
      fs.writeFileSync(path.join(work, "manifest.json"), `${JSON.stringify({
        tag: pin.tag, commit: pin.commit, revision: pin.revision, ...target, files: hashes,
      }, null, 2)}\n`);
      fs.rmSync(path.join(work, "bun-manifest.json"));
    } else {
      fs.writeFileSync(path.join(work, "manifest.json"), "{}\n");
    }
    fs.rmSync(destination, { recursive: true, force: true });
    fs.renameSync(work, destination);
  } finally {
    fs.rmSync(work, { recursive: true, force: true });
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const host = `${process.arch === "arm64" ? "aarch64" : process.arch === "x64" ? "x86_64" : process.arch}-${process.platform === "darwin" ? "apple-darwin" : process.platform === "linux" ? "unknown-linux-gnu" : process.platform === "win32" ? "pc-windows-msvc" : `unknown-${process.platform}`}`;
  stageRuntime(process.env.TAURI_ENV_TARGET_TRIPLE || host);
}
