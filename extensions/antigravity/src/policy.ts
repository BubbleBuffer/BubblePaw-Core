// What agy may do on its own: nothing except the `bubblepaw` MCP tools.

export const MCP_SERVER_NAME = "bubblepaw";
/** agy names an MCP tool `mcp_<server>_<tool>`. */
export const MCP_TOOL_PREFIX = `mcp_${MCP_SERVER_NAME}_`;
export const DENY_REASON = "Use the bubblepaw tools; built-in tools are disabled here.";

export type PermissionOutcome = { outcome: { outcome: "selected"; optionId: string } | { outcome: "cancelled" } };

/** Layer (a): the answer to `session/request_permission`. A `bubblepaw` tool is
 *  allowed at once (BubblePaw's own approval gate runs when the host dispatches the
 *  parked call); everything else is rejected. */
export function permissionOutcome(params: any): PermissionOutcome {
	const name = String(params?.toolCall?.name ?? "");
	const options: Array<{ optionId: string; kind: string }> = params?.options ?? [];
	const want = name.startsWith(MCP_TOOL_PREFIX) ? "allow" : "reject";
	const pick = options.find((o) => o.kind === `${want}_once`) ?? options.find((o) => o.kind.startsWith(want));
	return pick ? { outcome: { outcome: "selected", optionId: pick.optionId } } : { outcome: { outcome: "cancelled" } };
}

/** The hooks.json agy reads from its workspace's `.agents/` directory. */
export function hooksConfig(command: string): unknown {
	return {
		bubblepaw: {
			PreToolUse: [{ matcher: "*", hooks: [{ type: "command", command }] }],
		},
	};
}
