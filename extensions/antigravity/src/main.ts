// BubblePaw provider extension for Antigravity: JSON-RPC over stdio
// (docs/providers.md, "Out-of-process extensions" and "Antigravity provider").
//
// Environment:
//   BUBBLEPAW_AGY_BIN  The `agy` binary (default: `agy` on PATH).
//   BUBBLEPAW_AGY_DIR  A directory the extension owns: agy's workspace
//                      (`<dir>/workspace`, holding the PreToolUse hook config) and
//                      nothing else. Default: `<tmpdir>/bubblepaw-antigravity`.
//   BUBBLEPAW_AGY_HOOK `off` leaves the PreToolUse hook out (only the permission-request
//                      layer then denies built-in tools; for diagnosis, not for use).
//   BUBBLEPAW_AGY_LOG  Append one JSON line per finished step (model, usage).
//   GEMINI_API_KEY, GOOGLE_API_KEY, GOOGLE_APPLICATION_CREDENTIALS and the other
//                      variables in STRIPPED_ENV are dropped from the agy child
//                      unless BUBBLEPAW_EXTENSION_ENV_KEYS (set by the host from the
//                      instance's `env` table) names them, so an inherited key never
//                      switches agy off its cached login.

import { appendFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { AGY_PROMPT_TOKENS, capabilities, MODELS } from "./models.js";
import { AntigravityProvider, childEnv, type StepLog } from "./provider.js";
import { dispatchLine, encode, LineSplitter, notification, type Handlers, type Write } from "./rpc.js";
import { ProviderError, type ModelRequest } from "./types.js";

export function handlers(provider: AntigravityProvider, write: Write): Handlers {
	return {
		async request(method, params, id) {
			const text = (chunk: string) => write(encode(notification("text", { id, text: chunk })));
			switch (method) {
				case "initialize":
					return { provider: "antigravity", models: MODELS, capabilities: { ...capabilities(), output_schema: false, reasoning_effort: true, summary_fork: true, input_overhead_tokens: AGY_PROMPT_TOKENS } };
				case "step":
					return provider.step(params as ModelRequest, id, text);
				case "summary_fork":
					return provider.summaryFork(params as ModelRequest, id, text);
				case "session.close":
					if (typeof params?.session_id === "string") provider.closeSession(params.session_id);
					return {};
				default:
					throw new ProviderError("fatal", `unknown method ${method}`);
			}
		},
		notify(method, params) {
			if (method === "cancel") provider.cancel(params?.id);
		},
	};
}

/** Where the shim and the hook live, as agy is told to run them. */
export function launchers(execPath = process.execPath, srcDir = fileURLToPath(new URL(".", import.meta.url))) {
	const shim = join(srcDir, "mcp-shim.ts");
	const hook = join(srcDir, "hook.ts");
	return { shim: { command: execPath, args: [shim] }, hookCommand: `"${execPath}" "${hook}"` };
}

if (import.meta.main) {
	const log = (line: string) => process.stderr.write(`${line}\n`);
	const stepLog = process.env.BUBBLEPAW_AGY_LOG;
	const keep = (process.env.BUBBLEPAW_EXTENSION_ENV_KEYS ?? "").split(",").map((k) => k.trim()).filter(Boolean);
	const dir = process.env.BUBBLEPAW_AGY_DIR || join(tmpdir(), "bubblepaw-antigravity");
	const { shim, hookCommand } = launchers();
	const provider = new AntigravityProvider({
		command: process.env.BUBBLEPAW_AGY_BIN || "agy",
		args: ["acp", "--disable-telemetry"],
		cwd: join(dir, "workspace"),
		env: childEnv(process.env, keep),
		shim,
		hookCommand: process.env.BUBBLEPAW_AGY_HOOK === "off" ? undefined : hookCommand,
		log,
		onStep: (entry: StepLog) => {
			log(`step ${JSON.stringify(entry)}`);
			if (stepLog) appendFileSync(stepLog, `${JSON.stringify({ ...entry, at: new Date().toISOString() })}\n`);
		},
	});
	const write: Write = (line) => {
		process.stdout.write(line);
	};
	const h = handlers(provider, write);
	const splitter = new LineSplitter();
	process.stdin.setEncoding("utf8");
	process.stdin.on("data", (chunk: string) => {
		for (const line of splitter.push(chunk)) dispatchLine(line, h, write);
	});
	process.stdin.on("end", () => {
		for (const line of splitter.flush()) dispatchLine(line, h, write);
		provider.shutdown();
		setTimeout(() => process.exit(0), 200);
	});
	log("antigravity extension started");
}
