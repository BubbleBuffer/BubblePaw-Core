//! Architecture ratchet: dependencies and vocabulary must move down the layers.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
const ALLOWLIST: &str = "layering-allowlist.txt";
struct Rule {
    name: &'static str,
    allowed: &'static [&'static str],
    forbidden: &'static [&'static str],
    words: &'static [&'static str],
}
const RULES: &[Rule] = &[
    Rule {
        name: "bubblepaw-ai",
        allowed: &[],
        forbidden: &[],
        words: &[
            "session", "rusqlite", "approval", "episode", "chapter", "child",
        ],
    },
    Rule {
        name: "bubblepaw-agent",
        allowed: &["bubblepaw-ai"],
        forbidden: &["rusqlite", "reqwest"],
        words: &["episode", "chapter", "child", "opencode", "quota", "\"js\""],
    },
    Rule {
        name: "bubblepaw-core",
        allowed: &["bubblepaw-ai", "bubblepaw-agent"],
        forbidden: &["reqwest"],
        words: &["episode", "chapter", "child", "opencode", "quota", "\"js\""],
    },
    Rule {
        name: "bubblepaw-process",
        allowed: &[],
        forbidden: &[],
        words: &[],
    },
];
type Dependencies = BTreeMap<String, BTreeSet<String>>;
#[derive(Default)]
struct Allowlist {
    deps: BTreeSet<(String, String)>,
    words: BTreeMap<(String, String), usize>,
}
struct FileWords {
    krate: String,
    path: String,
    counts: BTreeMap<String, usize>,
}

/// Pure checker, shared by the fixtures and the filesystem guard.
fn check(
    rules: &[Rule],
    deps: &Dependencies,
    files: &[FileWords],
    allow: &Allowlist,
) -> Vec<String> {
    let mut messages = Vec::new();
    let mut violations = BTreeSet::new();
    for (krate, dependencies) in deps {
        let rule = rules.iter().find(|rule| rule.name == krate);
        for dependency in dependencies {
            let global = dependency == "bubblepaw-sdk"
                || (dependency == "bubblepaw-runner" && krate != "bubblepaw-sdk");
            let forbidden = rule.is_some_and(|rule| {
                (deps.contains_key(dependency) && !rule.allowed.contains(&dependency.as_str()))
                    || rule.forbidden.contains(&dependency.as_str())
            });
            if global || forbidden {
                violations.insert((krate.clone(), dependency.clone()));
            }
        }
    }
    for (krate, dependency) in violations.difference(&allow.deps) {
        messages.push(format!("{krate}: forbidden dependency {dependency}; move the dependency to its owning layer ({ALLOWLIST})"));
    }
    for (krate, dependency) in allow.deps.difference(&violations) {
        messages.push(format!(
            "stale dependency allowance: dep {krate} {dependency}; remove it from {ALLOWLIST}"
        ));
    }
    for rule in rules.iter().filter(|rule| deps.contains_key(rule.name)) {
        for &word in rule.words {
            let mut contributors: Vec<_> = files
                .iter()
                .filter(|file| file.krate == rule.name)
                .filter_map(|file| {
                    file.counts
                        .get(word)
                        .filter(|&&count| count > 0)
                        .map(|&count| (count, file.path.as_str()))
                })
                .collect();
            let total: usize = contributors.iter().map(|(count, _)| count).sum();
            let ceiling = allow
                .words
                .get(&(rule.name.to_string(), word.to_string()))
                .copied()
                .unwrap_or(0);
            if total > ceiling {
                contributors.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
                let top = contributors
                    .iter()
                    .take(10)
                    .map(|(count, path)| format!("  {path}: {count}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                messages.push(format!("{}: word {word:?} occurs {total} times, above ceiling {ceiling}; remove occurrences or move the code to its owning layer. Top files:\n{top}", rule.name));
            }
        }
    }
    messages
}
fn read_allowlist(text: &str) -> Allowlist {
    let mut allow = Allowlist::default();
    for (index, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or_default();
        let fields: Vec<_> = line.split_whitespace().collect();
        let valid = match fields.as_slice() {
            [] => true,
            ["dep", krate, dependency] => allow
                .deps
                .insert((krate.to_string(), dependency.to_string())),
            ["word", krate, word, ceiling] => {
                let ceiling = ceiling
                    .parse()
                    .unwrap_or_else(|_| panic!("{ALLOWLIST}:{}: invalid count", index + 1));
                allow
                    .words
                    .insert((krate.to_string(), word.to_string()), ceiling)
                    .is_none()
            }
            _ => false,
        };
        assert!(
            valid,
            "{ALLOWLIST}:{}: malformed or duplicate entry: {line}",
            index + 1
        );
    }
    allow
}

#[derive(Default)]
struct Dependency {
    package: Option<String>,
    workspace: bool,
}
#[derive(Default)]
struct Manifest {
    name: String,
    dependencies: BTreeMap<(Vec<String>, String), Dependency>,
    workspace: BTreeMap<String, Dependency>,
}

/// A small TOML lexer: quoted strings remain single tokens (including cfg headers),
/// comments disappear, and newlines delimit assignments outside arrays/tables.
fn tokens(text: &str) -> Vec<String> {
    let mut chars = text.chars().peekable();
    let mut out = Vec::new();
    while let Some(ch) = chars.next() {
        match ch {
            '#' => {
                while chars.peek().is_some_and(|&ch| ch != '\n') {
                    chars.next();
                }
            }
            '\n' => out.push("\n".into()),
            ch if ch.is_whitespace() => {}
            '"' | '\'' => {
                let mut value = String::new();
                loop {
                    let next = chars.next().expect("unterminated TOML string");
                    if next == ch {
                        break;
                    }
                    if next == '\\' && ch == '"' {
                        value.push(chars.next().expect("unterminated TOML escape"));
                    } else {
                        value.push(next);
                    }
                }
                out.push(value);
            }
            '[' | ']' | '{' | '}' | '=' | '.' | ',' => out.push(ch.to_string()),
            _ => {
                let mut value = ch.to_string();
                while chars
                    .peek()
                    .is_some_and(|ch| !ch.is_whitespace() && !"#[]{}=.,\"'".contains(*ch))
                {
                    value.push(chars.next().expect("peeked character"));
                }
                out.push(value);
            }
        }
    }
    out
}
fn manifest(text: &str) -> Manifest {
    let tokens = tokens(text);
    let mut result = Manifest::default();
    let mut section = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        if tokens[index] == "\n" {
            index += 1;
            continue;
        }
        if tokens[index] == "[" {
            section.clear();
            while index < tokens.len() && tokens[index] != "\n" {
                if !["[", "]", "."].contains(&tokens[index].as_str()) {
                    section.push(tokens[index].clone());
                }
                index += 1;
            }
            continue;
        }
        let start = index;
        let mut depth = 0_i32;
        while index < tokens.len() {
            match tokens[index].as_str() {
                "[" | "{" => depth += 1,
                "]" | "}" => depth -= 1,
                "\n" if depth == 0 => break,
                _ => {}
            }
            index += 1;
        }
        let statement = &tokens[start..index];
        let Some(equals) = statement.iter().position(|token| token == "=") else {
            continue;
        };
        let key: Vec<_> = statement[..equals]
            .iter()
            .filter(|token| token.as_str() != ".")
            .cloned()
            .collect();
        let value = &statement[equals + 1..];
        if section == ["package"] && key == ["name"] {
            result.name = value.first().expect("package name").clone();
        }
        let workspace = section.starts_with(&["workspace".into(), "dependencies".into()]);
        let group = if workspace {
            Some(1)
        } else if matches!(
            section.first().map(String::as_str),
            Some("dependencies" | "build-dependencies")
        ) {
            Some(0)
        } else if section.first().is_some_and(|part| part == "target")
            && matches!(
                section.get(2).map(String::as_str),
                Some("dependencies" | "build-dependencies")
            )
        {
            Some(2)
        } else {
            None
        };
        let Some(group) = group else {
            continue;
        };
        let (alias, field) = if let Some(alias) = section.get(group + 1) {
            (alias, key.first().map(String::as_str))
        } else {
            (
                key.first().expect("dependency name"),
                key.get(1).map(String::as_str),
            )
        };
        let dependency = if workspace {
            result.workspace.entry(alias.clone()).or_default()
        } else {
            let scope = section[..=group].to_vec();
            result
                .dependencies
                .entry((scope, alias.clone()))
                .or_default()
        };
        if let Some(field) = field {
            set_field(dependency, field, value.first().expect("dependency field"));
        } else {
            for fields in value.windows(3).filter(|fields| fields[1] == "=") {
                set_field(dependency, &fields[0], &fields[2]);
            }
        }
    }
    result
}
fn set_field(dependency: &mut Dependency, field: &str, value: &str) {
    match field {
        "package" => dependency.package = Some(value.to_string()),
        "workspace" => dependency.workspace = value == "true",
        _ => {}
    }
}
fn dependency_names(
    manifest: &Manifest,
    workspace: &BTreeMap<String, Dependency>,
) -> BTreeSet<String> {
    manifest
        .dependencies
        .iter()
        .map(|((_, alias), dependency)| {
            let inherited = dependency.workspace.then(|| {
                workspace
                    .get(alias)
                    .unwrap_or_else(|| panic!("missing workspace dependency {alias}"))
            });
            dependency
                .package
                .as_ref()
                .or_else(|| inherited.and_then(|dependency| dependency.package.as_ref()))
                .unwrap_or(alias)
                .clone()
        })
        .collect()
}
fn collect_sources(
    root: &Path,
    dir: &Path,
    krate: &str,
    words: &[&str],
    files: &mut Vec<FileWords>,
) {
    if !dir.exists() {
        return;
    }
    for entry in fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
    {
        let path = entry.expect("source directory entry").path();
        if path.is_dir() {
            collect_sources(root, &path, krate, words, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
                .to_lowercase();
            files.push(FileWords {
                krate: krate.to_string(),
                path: path
                    .strip_prefix(root)
                    .expect("source under root")
                    .to_string_lossy()
                    .replace('\\', "/"),
                counts: words
                    .iter()
                    .map(|&word| (word.to_string(), source.matches(word).count()))
                    .collect(),
            });
        }
    }
}
#[test]
fn workspace_respects_layers() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let read = |path: &Path| {
        fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    };
    let workspace = manifest(&read(&root.join("Cargo.toml"))).workspace;
    let mut deps = Dependencies::new();
    let mut files = Vec::new();
    for entry in fs::read_dir(root.join("crates")).expect("workspace crates directory") {
        let dir = entry.expect("crate directory entry").path();
        if !dir.join("Cargo.toml").is_file() {
            continue;
        }
        let parsed = manifest(&read(&dir.join("Cargo.toml")));
        assert!(
            !parsed.name.is_empty(),
            "missing package name: {}",
            dir.display()
        );
        deps.insert(parsed.name.clone(), dependency_names(&parsed, &workspace));
        if let Some(rule) = RULES.iter().find(|rule| rule.name == parsed.name) {
            collect_sources(
                &root,
                &dir.join("src"),
                &parsed.name,
                rule.words,
                &mut files,
            );
        }
    }
    assert!(deps.contains_key("bubblepaw-core"), "no core crate found");
    let messages = check(
        RULES,
        &deps,
        &files,
        &read_allowlist(&read(&root.join(ALLOWLIST))),
    );
    assert!(
        messages.is_empty(),
        "layering guard failed:\n{}",
        messages.join("\n")
    );
}
#[test]
fn checker_cases() {
    struct Case {
        name: &'static str,
        edges: &'static [(&'static str, &'static str)],
        allow: &'static str,
        count: usize,
        expected: &'static [&'static str],
    }
    #[rustfmt::skip]
    let cases = [
        Case { name: "legal edges and shrinking words", edges: &[("bubblepaw-core", "bubblepaw-agent"), ("bubblepaw-agent", "bubblepaw-ai"), ("bubblepaw-sdk", "bubblepaw-runner")], allow: "word bubblepaw-core child 3", count: 2, expected: &[] },
        Case { name: "upward edges", edges: &[("bubblepaw-ai", "bubblepaw-core"), ("bubblepaw-agent", "bubblepaw-core"), ("bubblepaw-core", "bubblepaw-process"), ("bubblepaw-process", "bubblepaw-ai")], allow: "", count: 0, expected: &["bubblepaw-ai: forbidden", "bubblepaw-agent: forbidden", "bubblepaw-core: forbidden", "bubblepaw-process: forbidden"] },
        Case { name: "external and global restrictions", edges: &[("bubblepaw-agent", "rusqlite"), ("bubblepaw-agent", "reqwest"), ("bubblepaw-core", "reqwest"), ("bubblepaw-mcp", "bubblepaw-sdk"), ("bubblepaw-mcp", "bubblepaw-runner")], allow: "", count: 0, expected: &["dependency rusqlite", "dependency reqwest", "dependency bubblepaw-sdk", "dependency bubblepaw-runner"] },
        Case { name: "allowance and stale entry", edges: &[("bubblepaw-core", "bubblepaw-process")], allow: "dep bubblepaw-core bubblepaw-process\ndep bubblepaw-core reqwest", count: 0, expected: &["stale dependency allowance"] },
        Case { name: "missing ceiling is zero", edges: &[], allow: "", count: 1, expected: &["word \"child\" occurs 1", "crates/bubblepaw-core/src/example.rs: 1"] },
        Case { name: "word count equal to ceiling", edges: &[], allow: "word bubblepaw-core child 2", count: 2, expected: &[] },
        Case { name: "allowlisted edge", edges: &[("bubblepaw-core", "bubblepaw-process")], allow: "dep bubblepaw-core bubblepaw-process", count: 0, expected: &[] },
    ];
    for case in cases {
        let mut deps: Dependencies = [
            "bubblepaw-ai",
            "bubblepaw-agent",
            "bubblepaw-core",
            "bubblepaw-process",
            "bubblepaw-runner",
            "bubblepaw-sdk",
            "bubblepaw-mcp",
        ]
        .into_iter()
        .map(|name| (name.to_string(), BTreeSet::new()))
        .collect();
        for &(krate, dependency) in case.edges {
            deps.get_mut(krate).unwrap().insert(dependency.to_string());
        }
        let files = [FileWords {
            krate: "bubblepaw-core".into(),
            path: "crates/bubblepaw-core/src/example.rs".into(),
            counts: [("child".into(), case.count)].into(),
        }];
        let result = check(RULES, &deps, &files, &read_allowlist(case.allow)).join("\n");
        assert_eq!(
            result.is_empty(),
            case.expected.is_empty(),
            "{}: {result}",
            case.name
        );
        for expected in case.expected {
            assert!(
                result.contains(expected),
                "{}: missing {expected:?}: {result}",
                case.name
            );
        }
    }
}
#[test]
fn dependency_parser_covers_cargo_forms() {
    let workspace = manifest(
        "[workspace.dependencies]\nshared = { package = 'bubblepaw-sdk', path = 'sdk' }\n",
    )
    .workspace;
    let parsed = manifest(
        r#"
[package]
name = "fixture"
[dependencies]
shared.workspace = true
renamed = { package = "bubblepaw-runner", path = "runner" }
multiline = {
    package = "bubblepaw-ai", # comment
    path = "ai"
}
[build-dependencies.build_alias]
package = "bubblepaw-agent"
path = "agent"
[target.'cfg(windows)'.dependencies]
platform = { package = "bubblepaw-process", path = "process" }
[target.'cfg(unix)'.build-dependencies.platform_build]
package = "bubblepaw-core"
[target.'cfg(unix)'.dev-dependencies]
ignored = { package = "ignored-target-dev" }
[dev-dependencies]
ignored = { package = "ignored-dev" }
"#,
    );
    assert_eq!(
        dependency_names(&parsed, &workspace),
        [
            "bubblepaw-sdk",
            "bubblepaw-runner",
            "bubblepaw-ai",
            "bubblepaw-agent",
            "bubblepaw-process",
            "bubblepaw-core"
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    );
}
