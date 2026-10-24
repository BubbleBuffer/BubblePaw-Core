import type { Options } from "@anthropic-ai/claude-agent-sdk";
import { ProviderError, type ModelRequest } from "./types.js";

type NativeOptions = Pick<Options, "thinking" | "effort" | "outputFormat">;

export interface RequestOptions {
	native: NativeOptions;
	/** Immutable identities for options the SDK accepts only when a query starts. */
	effort: string;
	schema: string | undefined;
}

function object(value: unknown): value is Record<string, unknown> {
	return value !== null && typeof value === "object" && !Array.isArray(value);
}

/** Match the core's closed-object validation at the extension's RPC boundary. */
function closedObjects(schema: unknown, path: string): void {
	if (!object(schema)) return;
	const isObject = schema.type === "object" || (Array.isArray(schema.type) && schema.type.includes("object"))
		|| "properties" in schema || "patternProperties" in schema || "additionalProperties" in schema;
	if (isObject && schema.additionalProperties !== false) {
		throw new ProviderError("fatal", `output_schema object at ${path} must set additionalProperties: false`);
	}
	for (const keyword of ["properties", "patternProperties", "$defs", "definitions", "dependentSchemas", "dependencies"]) {
		const children = schema[keyword];
		if (object(children)) for (const [name, child] of Object.entries(children)) closedObjects(child, `${path}/${keyword}/${name}`);
	}
	for (const keyword of ["items", "additionalItems", "unevaluatedItems", "contains", "contentSchema", "propertyNames", "not", "if", "then", "else", "unevaluatedProperties", "additionalProperties", "allOf", "anyOf", "oneOf", "prefixItems"]) {
		const child = schema[keyword];
		if (Array.isArray(child)) child.forEach((value, index) => closedObjects(value, `${path}/${keyword}/${index}`));
		else closedObjects(child, `${path}/${keyword}`);
	}
}

function canonical(value: unknown): unknown {
	if (Array.isArray(value)) return value.map(canonical);
	if (object(value)) return Object.fromEntries(Object.keys(value).sort().map((key) => [key, canonical(value[key])]));
	return value;
}

/** Validate before writing session files, starting a query or releasing effects. */
export function requestOptions(request: ModelRequest): RequestOptions {
	const effort = request.reasoning_effort ?? "low";
	if (!["low", "medium", "high", "xhigh", "max"].includes(effort)) {
		throw new ProviderError("fatal", "invalid reasoning_effort: expected low, medium, high, xhigh or max");
	}
	const native: NativeOptions = { thinking: { type: "adaptive" }, effort };
	let schema: string | undefined;
	if (request.output_schema != null) {
		const output = request.output_schema;
		if (!object(output) || typeof output.name !== "string" || output.name.length < 1 || output.name.length > 64 || /[^A-Za-z0-9_-]/u.test(output.name)) {
			throw new ProviderError("fatal", "output_schema name must be 1-64 ASCII letters, digits, underscores or dashes");
		}
		if (!object(output.schema)) throw new ProviderError("fatal", "output_schema must be a JSON schema object");
		closedObjects(output.schema, "$");
		// Freeze the request's value: later host mutations cannot change an active query.
		const cloned = structuredClone(output.schema);
		native.outputFormat = { type: "json_schema", schema: cloned };
		schema = JSON.stringify(canonical({ name: output.name, schema: cloned }));
	}
	return { native, effort, schema };
}

export function assertUnchangedOptions(previous: RequestOptions, next: RequestOptions): void {
	if (previous.effort !== next.effort) throw new ProviderError("fatal", "reasoning_effort cannot change while Claude Code is waiting for tool results");
	if (previous.schema !== next.schema) throw new ProviderError("fatal", "output_schema cannot change while Claude Code is waiting for tool results");
}
