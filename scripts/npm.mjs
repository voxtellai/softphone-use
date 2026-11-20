#!/usr/bin/env node
// Packages and versions for npm.
//
//   node scripts/npm.mjs version <x.y.z>                 set the version in Cargo.toml and package.json
//   node scripts/npm.mjs check <x.y.z>                   fail unless both already say <x.y.z>
//   node scripts/npm.mjs stage <artifacts> <out>         one package per platform, plus the main package
//   node scripts/npm.mjs publish <out>                   publish what stage made, platforms first; skips
//                                                        versions already on npm. CI adds provenance.
//
// `stage` expects <artifacts>/<platform>/softphone-use[.exe], one directory per entry in PLATFORMS.

import { execFileSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const main = join(root, "npm", "softphone-use");

/** npm platform key -> what npm matches on, and the Rust target it is built for. */
export const PLATFORMS = {
  "darwin-arm64": { os: "darwin", cpu: "arm64", target: "aarch64-apple-darwin" },
  "darwin-x64": { os: "darwin", cpu: "x64", target: "x86_64-apple-darwin" },
  "linux-arm64": { os: "linux", cpu: "arm64", libc: "glibc", target: "aarch64-unknown-linux-gnu" },
  "linux-x64": { os: "linux", cpu: "x64", libc: "glibc", target: "x86_64-unknown-linux-gnu" },
  "win32-x64": { os: "win32", cpu: "x64", target: "x86_64-pc-windows-msvc" },
};

const readJson = (p) => JSON.parse(readFileSync(p, "utf8"));
const writeJson = (p, v) => writeFileSync(p, `${JSON.stringify(v, null, 2)}\n`);

function cargoVersion() {
  return readFileSync(join(root, "Cargo.toml"), "utf8").match(/^version = "([^"]+)"/m)[1];
}

function setVersion(version) {
  if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) throw new Error(`not a version: ${version}`);
  const cargo = join(root, "Cargo.toml");
  writeFileSync(cargo, readFileSync(cargo, "utf8").replace(/^version = "[^"]+"/m, `version = "${version}"`));
  const pkg = readJson(join(main, "package.json"));
  pkg.version = version;
  for (const key of Object.keys(PLATFORMS)) pkg.optionalDependencies[`softphone-use-${key}`] = version;
  writeJson(join(main, "package.json"), pkg);
  // Keep Cargo.lock's own entry in step.
  const lock = join(root, "Cargo.lock");
  writeFileSync(lock, readFileSync(lock, "utf8").replace(/(name = "softphone-use"\nversion = )"[^"]+"/, `$1"${version}"`));
}

function check(version) {
  const pkg = readJson(join(main, "package.json")).version;
  const cargo = cargoVersion();
  if (pkg !== version || cargo !== version) {
    throw new Error(`tag says ${version}, Cargo.toml says ${cargo}, package.json says ${pkg}; run: pnpm release:version ${version}`);
  }
}

function stage(artifacts, out) {
  const pkg = readJson(join(main, "package.json"));
  const { version } = pkg;
  rmSync(out, { recursive: true, force: true });
  for (const [key, p] of Object.entries(PLATFORMS)) {
    const exe = p.os === "win32" ? "softphone-use.exe" : "softphone-use";
    const from = join(artifacts, key, exe);
    if (!existsSync(from)) throw new Error(`missing ${from}`);
    const dir = join(out, `softphone-use-${key}`);
    mkdirSync(join(dir, "bin"), { recursive: true });
    copyFileSync(from, join(dir, "bin", exe));
    chmodSync(join(dir, "bin", exe), 0o755);
    copyFileSync(join(root, "LICENSE"), join(dir, "LICENSE"));
    writeFileSync(join(dir, "README.md"), `# softphone-use-${key}\n\nThe ${key} binary of [softphone-use](https://www.npmjs.com/package/softphone-use). Install \`softphone-use\` instead.\n`);
    writeJson(join(dir, "package.json"), {
      name: `softphone-use-${key}`,
      version,
      description: `The ${key} binary of softphone-use.`,
      license: pkg.license,
      repository: pkg.repository,
      os: [p.os],
      cpu: [p.cpu],
      ...(p.libc ? { libc: [p.libc] } : {}),
      files: ["bin", "README.md", "LICENSE"],
      preferUnplugged: true,
    });
  }
  const dir = join(out, "softphone-use");
  mkdirSync(join(dir, "bin"), { recursive: true });
  copyFileSync(join(main, "bin", "softphone-use.js"), join(dir, "bin", "softphone-use.js"));
  copyFileSync(join(root, "README.md"), join(dir, "README.md"));
  copyFileSync(join(root, "LICENSE"), join(dir, "LICENSE"));
  writeJson(join(dir, "package.json"), pkg);
  console.log(`staged ${version} in ${out}`);
}

function published(name, version) {
  try {
    execFileSync("npm", ["view", `${name}@${version}`, "version"], { stdio: ["ignore", "pipe", "ignore"], shell: process.platform === "win32" });
    return true;
  } catch {
    return false;
  }
}

function publish(out) {
  const dirs = readdirSync(out).sort((a, b) => (a === "softphone-use") - (b === "softphone-use"));
  for (const d of dirs) {
    const { name, version } = readJson(join(out, d, "package.json"));
    if (published(name, version)) {
      console.log(`${name}@${version} is already on npm, skipping`);
      continue;
    }
    const tag = version.includes("-") ? "next" : "latest";
    console.log(`publishing ${name}@${version} (${tag})`);
    execFileSync("npm", ["publish", join(out, d), "--access", "public", "--tag", tag], { stdio: "inherit", shell: process.platform === "win32" });
  }
}

const [cmd, ...args] = process.argv.slice(2);
switch (cmd) {
  case "version":
    setVersion(args[0]);
    console.log(`version ${args[0]}: Cargo.toml, Cargo.lock and npm/softphone-use/package.json`);
    break;
  case "check":
    check(args[0]);
    break;
  case "stage":
    stage(args[0], args[1]);
    break;
  case "publish":
    publish(args[0]);
    break;
  default:
    console.error("usage: node scripts/npm.mjs version <x.y.z> | check <x.y.z> | stage <artifacts> <out> | publish <out>");
    process.exit(1);
}
