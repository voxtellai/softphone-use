#!/usr/bin/env node
// Runs the softphone-use binary that npm installed for this platform, as an optional
// dependency named softphone-use-<platform>-<arch>.

const { spawnSync } = require("node:child_process");

const target = `${process.platform}-${process.arch}`;
const exe = process.platform === "win32" ? "softphone-use.exe" : "softphone-use";

function binary() {
  if (process.env.SOFTPHONE_USE_BINARY) return process.env.SOFTPHONE_USE_BINARY;
  try {
    return require.resolve(`softphone-use-${target}/bin/${exe}`);
  } catch {
    console.error(
      `softphone-use: no binary for ${target}.\n` +
        "Prebuilt binaries cover macOS (arm64, x64), Linux with glibc (x64, arm64) and Windows (x64).\n" +
        "If your platform is one of these, reinstall without --no-optional (or --omit=optional).\n" +
        "Otherwise build from source: https://github.com/voxtellai/softphone-use#build-from-source",
    );
    process.exit(1);
  }
}

const result = spawnSync(binary(), process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`softphone-use: ${result.error.message}`);
  process.exit(1);
}
if (result.signal) {
  process.kill(process.pid, result.signal);
}
process.exit(result.status ?? 1);
