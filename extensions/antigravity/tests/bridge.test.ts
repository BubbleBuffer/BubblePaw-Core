import { expect, test } from "bun:test";
import net from "node:net";
import { Bridge } from "../src/bridge.js";

test("a null bridge message does not prevent the next valid request", () => {
	const bridge = new Bridge();
	const token = bridge.register({ tools: () => [], call: async () => ({ content: [] }) });
	const socket = new net.Socket();
	const replies: string[] = [];
	socket.write = ((data: string) => {
		replies.push(data);
		return true;
	}) as typeof socket.write;
	// Drive the socket callback directly: an uncaught callback exception must
	// fail this test without bringing down the rest of the provider test process.
	(bridge as unknown as { serve(socket: net.Socket): void }).serve(socket);
	try {
		expect(() => socket.emit("data", `null\n${JSON.stringify({ id: 1, token, op: "list" })}\n`)).not.toThrow();
		expect(replies.map((line) => JSON.parse(line))).toEqual([{ id: 1, tools: [] }]);
	} finally {
		socket.destroy();
		bridge.close();
	}
});
