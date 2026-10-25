import { describe, expect, test } from "bun:test";
import { createToolServer, ParkingLot } from "../src/mcp-server.js";

// The runner's todo definition (crates/bubblepaw-runner/src/todo.rs). The Rust
// test builtin_tool_schemas checks every built-in schema is an object; this one
// checks the extension serves todo with that shape.
const todo = {
	name: "todo",
	description: "Replace the entire todo list for this chat.",
	input_schema: {
		type: "object",
		properties: {
			items: {
				type: "array",
				maxItems: 50,
				items: {
					type: "object",
					properties: { text: { type: "string", minLength: 1, maxLength: 1024 }, status: { type: "string", enum: ["pending", "in_progress", "done"] } },
					required: ["text", "status"],
					additionalProperties: false,
				},
			},
		},
		required: ["items"],
		additionalProperties: false,
	},
};

describe("MCP tool schemas", () => {
	test("todo's object schema is served", () => {
		expect(() => createToolServer("bubblepaw", [todo], new ParkingLot())).not.toThrow();
	});

	test("a bare array schema is rejected naming the tool", () => {
		const bare = { ...todo, input_schema: todo.input_schema.properties.items };
		expect(() => createToolServer("bubblepaw", [bare], new ParkingLot())).toThrow(/^todo: MCP tool parameters must be an object schema/);
	});
});
