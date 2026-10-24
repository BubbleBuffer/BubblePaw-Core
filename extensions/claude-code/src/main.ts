// BubblePaw provider extension for Claude Code: JSON-RPC over stdio
// (docs/providers.md, "Out-of-process extensions" and "Claude Code provider").
//
// Part of bubblepaw-claude-code, whose provider mechanism is adapted from
// pi-claude-bridge (MIT, Copyright (c) 2026 Eli Dickinson); see NOTICE.
//
// Environment:
//   BUBBLEPAW_CC_CWD  Claude Code's working directory, which also decides where it
//                     keeps session files (default: this process's cwd).
//   BUBBLEPAW_CC_LOG  Append one JSON line per finished step (path, usage).
//   CLAUDE_CONFIG_DIR Claude Code's config dir: which logged-in account (subscription)
//                     the child uses, and where session files go.
//   BUBBLEPAW_CC_SYSTEM_PROMPT  `preset-append` (default): BubblePaw's prompt is
//                     appended to Claude Code's `claude_code` preset; `replace`:
//                     BubblePaw's prompt is the whole system prompt.
//   ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN are dropped from the Claude Code child
//                     unless BUBBLEPAW_EXTENSION_ENV_KEYS (set by the host from the
//                     instance's `env` table) names them, so an inherited key never
//                     switches an instance to API billing.

import { MODELS } from "./models.js";
import { ClaudeCodeProvider, fileStepLogger, parseSystemPromptMode } from "./provider.js";
import { dispatchLine, encode, LineSplitter, notification, type Handlers, type Write } from "./rpc.js";
import { ProviderError, type ModelRequest } from "./types.js";

export function handlers(provider: ClaudeCodeProvider, write: Write): Handlers {
	return {
		async request(method, params, id) {
			switch (method) {
				case "initialize":
					return { provider: "claude-code", models: MODELS, capabilities: { output_schema: true, reasoning_effort: true } };
				case "step":
					return provider.step(params as ModelRequest, id, (text) => write(encode(notification("text", { id, text }))));
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

if (import.meta.main) {
	const log = (line: string) => process.stderr.write(`${line}\n`);
	const stepLog = process.env.BUBBLEPAW_CC_LOG ? fileStepLogger(process.env.BUBBLEPAW_CC_LOG) : undefined;
	const provider = new ClaudeCodeProvider({
		cwd: process.env.BUBBLEPAW_CC_CWD || process.cwd(),
		claudeDir: process.env.CLAUDE_CONFIG_DIR,
		systemPromptMode: parseSystemPromptMode(process.env.BUBBLEPAW_CC_SYSTEM_PROMPT),
		log,
		onStep: (entry) => {
			log(`step ${JSON.stringify(entry)}`);
			stepLog?.(entry);
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
		for (const session of [...provider.lives.keys()]) provider.closeSession(session);
		setTimeout(() => process.exit(0), 200);
	});
	log("claude-code extension started");
}
