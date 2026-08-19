// Compiles the sidecar to a single self-contained binary named with the Rust
// target triple, as Tauri's externalBin requires: <name>-<triple>[.exe].
import { $ } from "bun";
import { mkdir } from "node:fs/promises";

const outDir = "src-tauri/binaries";
await mkdir(outDir, { recursive: true });

// Ask rustc for the host triple so the name matches what Tauri looks for.
const triple = (await $`rustc -vV`.text())
  .split("\n")
  .find((l) => l.startsWith("host:"))!
  .replace("host:", "")
  .trim();

const ext = process.platform === "win32" ? ".exe" : "";
const out = `${outDir}/chatmi-sidecar-${triple}${ext}`;

await $`bun build sidecar/index.ts --compile --outfile ${out}`;
console.log(`built ${out}`);
