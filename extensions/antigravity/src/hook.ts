// The PreToolUse hook agy runs before every tool call (layer (b) of the built-in
// tool denial): anything that is not a `bubblepaw` MCP tool is denied, with a
// reason the model sees. Reads agy's hook JSON on stdin, writes the decision.

import { DENY_REASON, MCP_TOOL_PREFIX } from "./policy.js";

let input = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk: string) => (input += chunk));
process.stdin.on("end", () => {
	let name = "";
	try {
		name = String(JSON.parse(input)?.toolCall?.name ?? "");
	} catch {}
	process.stdout.write(JSON.stringify(name.startsWith(MCP_TOOL_PREFIX) ? { decision: "allow" } : { decision: "deny", reason: DENY_REASON }));
});
