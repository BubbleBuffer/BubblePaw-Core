use super::*;

/// The `js` description for one registry: a short how-to with worked examples,
/// call signatures or short lines for always-shown capabilities, and every
/// other capability by name only (discovered with `tools.$search`).
/// Deterministic for the same capability set.
pub(super) fn js_reference(registry: &ToolRegistry) -> String {
    let capabilities = registry.capabilities();
    let has = |name: &str| capabilities.iter().any(|tool| tool.name == name);
    let mut description = String::from(
        r#"Run JavaScript (QuickJS, not Node) that composes available host tools. Await every tools.* call. The cell's value is its last expression or what it returns. Locals are fresh each cell; `state` keeps values between cells. console.log lines come back with the result.

Working well:
- Put independent calls in one cell with Promise.all; await dependent calls in order.
- Use file tools to read, search and edit; return what you need to see next.
- A failed or cancelled cell clears `state`.
"#,
    );
    if has("read") {
        description.push_str("- Results over the reply budget include fullOutput.path; read that path with tools.read.\n");
    }
    if has("exec") {
        description.push_str("- Use argv for single programs; cmd`prog ${value}` builds literal arguments. Check exitCode. Use strings only for pipes, redirects and && chains. Write code to a scratchpad file, then run it in the same batch: no python -c, node -e or inline PowerShell. PowerShell uses -NoProfile -File.\n");
        if has("wait") {
            description.push_str("- After starting background work, keep working and then call wait({handle}). Don't poll in a loop.\n");
        }
    }
    description.push_str("\nExamples:\n");
    if has("read") && has("search") {
        description.push_str("const [lib, hits] = await Promise.all([tools.read({path:'src/lib.rs'}), tools.search({query:'fn parse_config'})]);\nreturn {lib:lib.content,hits};\n");
    } else if has("read") {
        description.push_str("return await tools.read({path:'README.md'});\n");
    } else {
        description.push_str("return await tools.$list();\n");
    }
    if has("read") && has("edit") {
        description.push_str("await tools.edit({path:'src/config.rs',old_string:'let limit = 200;',new_string:'let limit = 1000;'});\n");
    }
    if has("exec") && has("wait") {
        description.push_str("const job = await tools.exec({command:{parts:['cargo test -j2'],values:[]},background:true});\n// ...other work here or in later cells...\nreturn await wait({handle:job.handle});\n");
    }
    description.push('\n');
    let mut deferred = Vec::new();
    for capability in &capabilities {
        if !registry.is_shown_always(&capability.name) {
            // `wait` is a global, not a tools.* member.
            if capability.name != "wait" {
                deferred.push(capability.name.as_str());
            }
            continue;
        }
        description.push_str(&signature(capability));
        description.push('\n');
    }
    if !deferred.is_empty() {
        deferred.sort_unstable();
        description.push_str(&format!(
            "Other tools (load the schema with tools.$search(\"select:name\")): {}\n",
            deferred.join(", ")
        ));
    }
    let mut servers = capabilities
        .iter()
        .filter_map(|tool| registry.tool_server(&tool.name))
        .collect::<Vec<_>>();
    servers.sort_unstable();
    servers.dedup();
    if !servers.is_empty() {
        description.push_str(&format!(
            "MCP servers (search their names with tools.$search): {}\n",
            servers.join(", ")
        ));
    }
    description.push_str(
        "Limits per cell: 3 s of active JS (awaiting doesn't count), 32 MiB heap, stopped after 30 s idle without a wait. No filesystem or network globals; use the tools.",
    );
    description
}
/// `tools.name({a, b?}) -> JSON: description`, from the input schema.
fn signature(definition: &ToolDefinition) -> String {
    let schema = &definition.input_schema;
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let fields: Vec<String> = schema["properties"]
        .as_object()
        .map(|properties| {
            properties
                .keys()
                .map(|key| {
                    if required.contains(&key.as_str()) {
                        key.clone()
                    } else {
                        format!("{key}?")
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let description = tool_definition_summary(&definition.description)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let name = if definition.name == "wait" {
        "wait".to_owned()
    } else {
        format!("tools.{}", definition.name)
    };
    format!("{name}({{{}}}) -> JSON: {description}", fields.join(", "))
}
