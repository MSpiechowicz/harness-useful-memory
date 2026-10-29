//! The manifests the resolvers need: Rust crate names (`Cargo.toml`), Go module paths (`go.mod`),
//! TypeScript path aliases (every `tsconfig*.json` and `jsconfig*.json`, following a bounded `extends` chain
//! and the project `references`, plus the `$lib` of a SvelteKit app) and Python source roots (found from the
//! paths of the Python files themselves).
//!
//! A manifest is read only when it is one of the files the walk found, so `extends` never leaves the
//! repository. A package name that is not a manifest of the repository (`@tsconfig/svelte`) is external
//! and ignored.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::pace::Pace;
use super::walk::{self, Class, Entry, Read};

/// How many `extends` links are followed.
const MAX_EXTENDS_DEPTH: usize = 5;

/// How many config files one alias lookup may read.
const MAX_CONFIG_FILES: usize = 16;

/// How many Python source roots a repository keeps (the shallowest win).
const MAX_PYTHON_ROOTS: usize = 64;

/// One `packages` row.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Package {
    /// `crate`, `gomod`, `alias` or `pyroot`.
    pub kind: &'static str,
    /// The crate name (with `_`), the Go module path, the alias prefix (`$lib`) or the Python root folder.
    pub name: String,
    /// The folder the package lives in, relative to the root (empty for the root itself).
    pub dir: String,
}

/// The packages the manifests among `entries` (found by the walk of `root`) declare, sorted.
pub fn collect(root: &Path, entries: &[Entry], pace: &Pace) -> Result<Vec<Package>, String> {
    let manifests: HashMap<&str, &Entry> = entries
        .iter()
        .filter(|entry| entry.class == Class::Package)
        .map(|entry| (entry.rel.as_str(), entry))
        .collect();
    let mut packages = Vec::new();
    let mut configs = Configs::new(root);

    for (rel, entry) in &manifests {
        pace.check()?;

        let name = rel.rsplit('/').next().unwrap_or(rel);

        match name {
            "Cargo.toml" => {
                if let Some(crate_name) = read(root, entry).as_deref().and_then(cargo_name) {
                    packages.push(Package {
                        kind: "crate",
                        name: crate_name,
                        dir: dir_of(rel).to_string(),
                    });
                }
            }
            "go.mod" => {
                if let Some(module) = read(root, entry).as_deref().and_then(go_module) {
                    packages.push(Package {
                        kind: "gomod",
                        name: module,
                        dir: dir_of(rel).to_string(),
                    });
                }
            }
            name if walk::is_ts_config(name) => packages.extend(ts_aliases(rel, &manifests, &mut configs)),
            _ => {}
        }
    }

    packages.extend(sveltekit_lib(entries, &packages));
    packages.extend(python_roots(entries));
    packages.sort();
    packages.dedup();

    Ok(packages)
}

/// `$lib` -> `<app>/src/lib` for each SvelteKit app (a folder with a `svelte.config.*`) whose configs define
/// no `$lib` inside it: SvelteKit adds that alias itself, in the generated `.svelte-kit/tsconfig.json`,
/// which the walk does not enter.
fn sveltekit_lib(entries: &[Entry], found: &[Package]) -> Vec<Package> {
    entries
        .iter()
        .filter(|entry| entry.rel.rsplit('/').next().is_some_and(is_svelte_config))
        .map(|entry| dir_of(&entry.rel))
        .filter(|app| {
            !found.iter().any(|package| {
                package.kind == "alias"
                    && package.name == "$lib"
                    && (app.is_empty() || package.dir.strip_prefix(*app).is_some_and(|rest| rest.starts_with('/')))
            })
        })
        .map(|app| Package {
            kind: "alias",
            name: "$lib".to_string(),
            dir: normalize(&format!("{app}/src/lib")),
        })
        .collect()
}

/// `svelte.config.js`, `.ts`, `.mjs`, `.cjs`.
fn is_svelte_config(name: &str) -> bool {
    name.strip_prefix("svelte.config.")
        .is_some_and(|ext| matches!(ext, "js" | "ts" | "mjs" | "cjs"))
}

/// The folders Python imports start from, when the walk found Python files: the repository root, `src/`
/// when a file lives there, and the folder above each top-level package (a folder with an `__init__.py`
/// whose parent has none). The shallowest `MAX_PYTHON_ROOTS` are kept.
fn python_roots(entries: &[Entry]) -> Vec<Package> {
    let sources: Vec<&str> = entries
        .iter()
        .filter(|entry| entry.class == Class::Source("python"))
        .map(|entry| entry.rel.as_str())
        .collect();

    if sources.is_empty() {
        return Vec::new();
    }

    let package_dirs: HashSet<&str> = sources
        .iter()
        .filter_map(|rel| {
            if *rel == "__init__.py" {
                Some("")
            } else {
                rel.strip_suffix("/__init__.py")
            }
        })
        .collect();
    let mut roots: BTreeSet<&str> = BTreeSet::new();

    roots.insert("");

    if sources.iter().any(|rel| rel.starts_with("src/")) {
        roots.insert("src");
    }

    for dir in package_dirs.iter().filter(|dir| !dir.is_empty()) {
        let parent = dir_of(dir);

        if !package_dirs.contains(parent) {
            roots.insert(parent);
        }
    }

    let mut roots: Vec<&str> = roots.into_iter().collect();

    roots.sort_by_key(|root| (root.matches('/').count(), *root));
    roots.truncate(MAX_PYTHON_ROOTS);

    roots
        .into_iter()
        .map(|root| Package {
            kind: "pyroot",
            name: root.to_string(),
            dir: root.to_string(),
        })
        .collect()
}

/// A short stable digest of the packages: a change moves resolution, so it moves the generation.
pub fn digest(packages: &[Package]) -> String {
    let mut text = String::new();

    for package in packages {
        text.push_str(package.kind);
        text.push('\t');
        text.push_str(&package.name);
        text.push('\t');
        text.push_str(&package.dir);
        text.push('\n');
    }

    super::store::digest_hex(text.as_bytes())
}

fn read(root: &Path, entry: &Entry) -> Option<String> {
    match walk::read_text(root, entry) {
        Read::Text(text) => Some(text),
        Read::Skipped(_) => None,
    }
}

fn dir_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// The crate name of a `Cargo.toml`: `[lib] name`, else `[package] name`, with `-` as `_`.
fn cargo_name(source: &str) -> Option<String> {
    let table: toml::Table = source.parse().ok()?;
    let name = |section: &str| {
        table
            .get(section)
            .and_then(|value| value.get("name"))
            .and_then(|value| value.as_str())
    };

    name("lib")
        .or_else(|| name("package"))
        .map(|name| name.replace('-', "_"))
        .filter(|name| !name.is_empty())
}

/// The module path of a `go.mod`.
fn go_module(source: &str) -> Option<String> {
    source.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("module")?;

        if !rest.starts_with(char::is_whitespace) {
            return None;
        }

        let module = rest.split("//").next().unwrap_or("").trim().trim_matches(['"', '`']);

        (!module.is_empty()).then(|| module.to_string())
    })
}

/// What a config file contributes to path aliases, before and after its `extends` chain.
#[derive(Default)]
struct Config {
    /// `paths` and the folder they are relative to when there is no `baseUrl`.
    paths: Option<(Vec<(String, String)>, String)>,
    /// `baseUrl`, as a folder relative to the root.
    base_url: Option<String>,
}

/// What one config file says by itself, kept small: a repository of thousands of configs that extend each
/// other would otherwise read and parse the same files again for every one of them.
#[derive(Clone, Default)]
struct Own {
    extends: Vec<String>,
    /// The `path` of each project reference.
    references: Vec<String>,
    base_url: Option<String>,
    paths: Option<Vec<(String, String)>>,
}

/// The config files read so far in this refresh, by path (`None` = unreadable or not JSON).
struct Configs {
    root: PathBuf,
    own: HashMap<String, Option<Own>>,
}

impl Configs {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            own: HashMap::new(),
        }
    }

    fn get(&mut self, rel: &str, manifests: &HashMap<&str, &Entry>) -> Option<Own> {
        if let Some(known) = self.own.get(rel) {
            return known.clone();
        }

        let parsed = manifests
            .get(rel)
            .and_then(|entry| read(&self.root, entry))
            .and_then(|text| serde_json::from_str::<Value>(&strip_jsonc(&text)).ok())
            .map(|value| own_of(&value, rel));

        self.own.insert(rel.to_string(), parsed.clone());

        parsed
    }
}

/// The most `extends` targets and `paths` entries of one config file that are kept.
const MAX_CONFIG_LISTED: usize = 64;

fn own_of(value: &Value, rel: &str) -> Own {
    let dir = dir_of(rel);
    let extends: Vec<String> = match value.get("extends") {
        Some(Value::String(one)) => vec![one.clone()],
        Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        _ => Vec::new(),
    };
    let references: Vec<String> = value
        .get("references")
        .and_then(Value::as_array)
        .map(|many| {
            many.iter()
                .filter_map(|reference| reference.get("path").and_then(Value::as_str))
                .map(str::to_string)
                .take(MAX_CONFIG_LISTED)
                .collect()
        })
        .unwrap_or_default();
    let options = value.get("compilerOptions");
    let base_url = options
        .and_then(|o| o.get("baseUrl"))
        .and_then(Value::as_str)
        .map(|base_url| normalize(&format!("{dir}/{base_url}")));
    let paths = options
        .and_then(|o| o.get("paths"))
        .and_then(Value::as_object)
        .map(|paths| {
            paths
                .iter()
                .filter_map(|(key, targets)| {
                    let first = targets.as_array()?.first()?.as_str()?;

                    Some((key.clone(), first.to_string()))
                })
                .take(MAX_CONFIG_LISTED)
                .collect()
        });

    Own {
        extends: extends.into_iter().take(MAX_CONFIG_LISTED).collect(),
        references,
        base_url,
        paths,
    }
}

/// The aliases `rel` defines, and those of the configs it references (a Vite app keeps its `paths` in
/// `tsconfig.app.json`, which `tsconfig.json` only references).
fn ts_aliases(rel: &str, manifests: &HashMap<&str, &Entry>, configs: &mut Configs) -> Vec<Package> {
    let mut aliases = Vec::new();
    let mut seen = Vec::new();
    let mut pending = vec![(rel.to_string(), 0)];

    while let Some((current, depth)) = pending.pop() {
        if seen.len() >= MAX_CONFIG_FILES || seen.contains(&current) {
            continue;
        }

        seen.push(current.clone());

        if depth < MAX_EXTENDS_DEPTH
            && let Some(own) = configs.get(&current, manifests)
        {
            let dir = dir_of(&current);

            for reference in own.references.iter().filter(|reference| !reference.starts_with('/')) {
                if let Some(target) = reference_target(&normalize(&format!("{dir}/{reference}")), manifests) {
                    pending.push((target, depth + 1));
                }
            }
        }

        aliases.extend(aliases_of(&current, manifests, configs));
    }

    aliases
}

/// The config file a project reference names: the file itself, with `.json` added, or the `tsconfig.json`
/// of the folder.
fn reference_target(path: &str, manifests: &HashMap<&str, &Entry>) -> Option<String> {
    [
        path.to_string(),
        format!("{path}.json"),
        format!("{path}/tsconfig.json"),
    ]
    .into_iter()
    .find(|candidate| manifests.contains_key(candidate.as_str()))
}

fn aliases_of(rel: &str, manifests: &HashMap<&str, &Entry>, configs: &mut Configs) -> Vec<Package> {
    let mut seen = Vec::new();
    let config = load_config(rel, manifests, 0, &mut seen, configs);
    let Some((paths, paths_dir)) = config.paths else {
        return Vec::new();
    };
    let base = config.base_url.unwrap_or(paths_dir);

    paths
        .into_iter()
        .map(|(key, target)| {
            let prefix = key.trim_end_matches('*').trim_end_matches('/').to_string();
            let target = target.trim_end_matches('*').trim_end_matches('/');

            Package {
                kind: "alias",
                name: prefix,
                dir: normalize(&format!("{base}/{target}")),
            }
        })
        .collect()
}

fn load_config(
    rel: &str,
    manifests: &HashMap<&str, &Entry>,
    depth: usize,
    seen: &mut Vec<String>,
    configs: &mut Configs,
) -> Config {
    let mut config = Config::default();

    if seen.len() >= MAX_CONFIG_FILES || seen.iter().any(|done| done == rel) {
        return config;
    }

    seen.push(rel.to_string());

    let Some(own) = configs.get(rel, manifests) else {
        return config;
    };
    let dir = dir_of(rel);

    if depth < MAX_EXTENDS_DEPTH {
        for target in own.extends.iter().filter(|target| target.starts_with('.')) {
            let mut resolved = normalize(&format!("{dir}/{target}"));

            if !manifests.contains_key(resolved.as_str()) {
                resolved.push_str(".json");
            }

            if manifests.contains_key(resolved.as_str()) {
                let parent = load_config(&resolved, manifests, depth + 1, seen, configs);

                config.paths = parent.paths.or(config.paths);
                config.base_url = parent.base_url.or(config.base_url);
            }
        }
    }

    if own.base_url.is_some() {
        config.base_url = own.base_url;
    }

    if let Some(list) = own.paths {
        config.paths = Some((list, dir.to_string()));
    }

    config
}

/// Resolves `.` and `..` in a `/`-separated relative path; `..` never climbs above the root.
pub fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();

    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }

    parts.join("/")
}

/// JSON with comments and trailing commas (what `tsconfig.json` allows) as plain JSON.
fn strip_jsonc(source: &str) -> String {
    let mut plain = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut in_string = false;

    while let Some(c) = chars.next() {
        if in_string {
            plain.push(c);

            if c == '\\' {
                plain.extend(chars.next());
            } else if c == '"' {
                in_string = false;
            }

            continue;
        }

        match (c, chars.peek().copied()) {
            ('"', _) => {
                in_string = true;
                plain.push(c);
            }
            ('/', Some('/')) => {
                for skipped in chars.by_ref() {
                    if skipped == '\n' {
                        plain.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut previous = ' ';

                for skipped in chars.by_ref() {
                    if previous == '*' && skipped == '/' {
                        break;
                    }

                    previous = skipped;
                }

                plain.push(' ');
            }
            _ => plain.push(c),
        }
    }

    drop_trailing_commas(&plain)
}

fn drop_trailing_commas(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut in_string = false;
    let mut i = 0;

    while let Some(&c) = chars.get(i) {
        i += 1;

        if in_string {
            out.push(c);

            if c == '\\' {
                if let Some(&escaped) = chars.get(i) {
                    out.push(escaped);
                    i += 1;
                }
            } else if c == '"' {
                in_string = false;
            }

            continue;
        }

        if c == '"' {
            in_string = true;
        }

        if c == ',' {
            let next = chars.get(i..).and_then(|rest| rest.iter().find(|n| !n.is_whitespace()));

            if matches!(next, Some('}' | ']')) {
                continue;
            }
        }

        out.push(c);
    }

    out
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn entries(dir: &std::path::Path) -> (PathBuf, Vec<Entry>) {
        (dir.to_path_buf(), walk::walk(dir, &|| false).expect("walk").entries)
    }

    fn gather((root, entries): &(PathBuf, Vec<Entry>)) -> Vec<Package> {
        let never = || false;

        collect(root, entries, &Pace::new(&never, std::time::Duration::from_secs(60))).expect("collect")
    }

    #[test]
    fn crate_and_module_names_are_read() {
        assert_eq!(
            cargo_name("[package]\nname = \"harness-memory\"\nversion = \"1\"\n").as_deref(),
            Some("harness_memory")
        );
        assert_eq!(
            cargo_name("[package]\nname = \"a-b\"\n[lib]\nname = \"core_lib\"\n").as_deref(),
            Some("core_lib")
        );
        assert_eq!(cargo_name("[workspace]\nmembers = [\"a\"]\n"), None);
        assert_eq!(cargo_name("not toml ["), None);
        assert_eq!(
            go_module("module example.com/app // c\n\ngo 1.22\n").as_deref(),
            Some("example.com/app")
        );
        assert_eq!(go_module("modulefoo\n"), None);
    }

    #[test]
    fn jsonc_comments_and_trailing_commas_are_stripped() {
        let plain = strip_jsonc("{\n // c\n \"a\": \"x//y\", /* b */ \"b\": [1, 2,],\n}\n");
        let value: Value = serde_json::from_str(&plain).expect("json");

        assert_eq!(value["a"], "x//y");
        assert_eq!(value["b"][1], 2);
    }

    #[test]
    fn normalize_resolves_dots_and_never_climbs_out() {
        assert_eq!(normalize("a/./b/../c"), "a/c");
        assert_eq!(normalize("../x/y"), "x/y");
        assert_eq!(normalize("./"), "");
    }

    #[test]
    fn aliases_follow_extends_and_base_url() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        fs::create_dir_all(root.join("app")).expect("mkdir");
        fs::write(
            root.join("tsconfig.base.json"),
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@lib/*": ["packages/lib/src/*"] } } }"#,
        )
        .expect("write");
        fs::write(root.join("app/tsconfig.json"), r#"{ "extends": "../tsconfig.base" }"#).expect("write");
        fs::write(
            root.join("jsconfig.json"),
            "{ // comment\n \"compilerOptions\": { \"paths\": { \"$lib/*\": [\"./src/lib/*\"], }, }, }",
        )
        .expect("write");
        // a cycle and a package name are tolerated
        fs::write(
            root.join("tsconfig.loop.json"),
            r#"{ "extends": "./tsconfig.loop.json" }"#,
        )
        .expect("write");
        fs::write(
            root.join("tsconfig.json"),
            r#"{ "extends": ["@tsconfig/svelte", "./tsconfig.loop.json"] }"#,
        )
        .expect("write");

        let found = gather(&entries(root));
        let aliases: Vec<(&str, &str)> = found
            .iter()
            .filter(|p| p.kind == "alias")
            .map(|p| (p.name.as_str(), p.dir.as_str()))
            .collect();

        assert_eq!(aliases, vec![("$lib", "src/lib"), ("@lib", "packages/lib/src")]);
    }

    fn aliases(found: &[Package]) -> Vec<(&str, &str)> {
        found
            .iter()
            .filter(|p| p.kind == "alias")
            .map(|p| (p.name.as_str(), p.dir.as_str()))
            .collect()
    }

    #[test]
    fn aliases_come_from_every_config_and_from_the_configs_a_solution_config_references() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();

        // a Vite app: the root config only references the two that hold options
        fs::create_dir_all(root.join("web/config")).expect("mkdir");
        fs::write(
            root.join("web/tsconfig.json"),
            r#"{ "files": [], "references": [{ "path": "./tsconfig.app.json" }, { "path": "./config/tsconfig.node" }, { "path": "./config" }] }"#,
        )
        .expect("write");
        fs::write(
            root.join("web/tsconfig.app.json"),
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@/*": ["./src/*"] } } }"#,
        )
        .expect("write");
        fs::write(
            root.join("web/config/tsconfig.node.json"),
            r#"{ "compilerOptions": { "paths": { "~tools/*": ["../tools/*"] } } }"#,
        )
        .expect("write");
        fs::write(
            root.join("web/config/tsconfig.json"),
            r#"{ "compilerOptions": { "paths": { "~cfg/*": ["./*"] } }, "references": [{ "path": ".." }] }"#,
        )
        .expect("write");

        let found = gather(&entries(root));

        assert_eq!(
            aliases(&found),
            vec![("@", "web/src"), ("~cfg", "web/config"), ("~tools", "web/tools")]
        );
    }

    #[test]
    fn a_sveltekit_app_gets_lib_unless_a_config_defines_it() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();

        fs::create_dir_all(root.join("site/.svelte-kit")).expect("mkdir");
        fs::create_dir_all(root.join("docs")).expect("mkdir");
        fs::write(root.join("site/svelte.config.js"), "export default {};\n").expect("write");
        // the generated config of a SvelteKit app lives in a folder the walk does not enter
        fs::write(
            root.join("site/.svelte-kit/tsconfig.json"),
            r#"{ "compilerOptions": { "paths": { "$lib": ["../src/lib"], "$lib/*": ["../src/lib/*"] } } }"#,
        )
        .expect("write");
        fs::write(root.join("docs/svelte.config.ts"), "export default {};\n").expect("write");

        let found = gather(&entries(root));

        assert_eq!(
            aliases(&found),
            vec![("$lib", "docs/src/lib"), ("$lib", "site/src/lib")]
        );

        fs::write(
            root.join("docs/tsconfig.json"),
            r#"{ "compilerOptions": { "paths": { "$lib/*": ["./custom/*"] } } }"#,
        )
        .expect("write");

        let found = gather(&entries(root));

        // each app on its own: `docs` defines its `$lib`, `site` keeps the default
        assert_eq!(aliases(&found), vec![("$lib", "docs/custom"), ("$lib", "site/src/lib")]);
    }

    #[test]
    fn python_roots_are_the_repository_src_and_the_folders_above_top_level_packages() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();

        for file in [
            "tool.py",
            "src/lib/__init__.py",
            "src/lib/sub/__init__.py",
            "libs/shared/core/__init__.py",
            "libs/shared/core/inner/__init__.py",
            "scripts/run.py",
        ] {
            fs::create_dir_all(root.join(file).parent().expect("parent")).expect("mkdir");
            fs::write(root.join(file), "x = 1\n").expect("write");
        }

        let found = gather(&entries(root));
        let roots: Vec<&str> = found
            .iter()
            .filter(|p| p.kind == "pyroot")
            .map(|p| p.dir.as_str())
            .collect();

        assert_eq!(roots, vec!["", "libs/shared", "src"]);

        let none = tempfile::tempdir().expect("dir");
        fs::write(none.path().join("a.rs"), "fn a() {}\n").expect("write");

        assert!(gather(&entries(none.path())).iter().all(|p| p.kind != "pyroot"));
    }

    #[test]
    fn the_digest_changes_with_the_packages() {
        let one = vec![Package {
            kind: "crate",
            name: "a".into(),
            dir: String::new(),
        }];
        let mut two = one.clone();
        two[0].dir = "x".into();

        assert_eq!(digest(&one), digest(&one.clone()));
        assert_ne!(digest(&one), digest(&two));
        assert_eq!(digest(&one).len(), 32);
    }

    #[test]
    fn a_shared_base_config_is_read_once_however_many_configs_extend_it() {
        let dir = tempfile::tempdir().expect("dir");
        let filler = format!("\"filler\": [{}]", vec!["\"x\""; 100_000].join(","));
        let base = format!(
            "{{ {filler}, \"compilerOptions\": {{ \"baseUrl\": \".\", \"paths\": {{ \"$lib/*\": [\"lib/*\"] }} }} }}"
        );

        fs::write(dir.path().join("tsconfig.base.json"), base).expect("write");

        for n in 0..400 {
            let folder = dir.path().join(format!("app{n}"));

            fs::create_dir_all(&folder).expect("mkdir");
            fs::write(
                folder.join("tsconfig.json"),
                "{ \"extends\": \"../tsconfig.base.json\" }",
            )
            .expect("write");
        }

        let started = std::time::Instant::now();
        let found = gather(&entries(dir.path()));

        // the base used to be read and parsed again for each of the 400
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            found
                .iter()
                .filter(|p| p.kind == "alias")
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            vec!["$lib"]
        );
    }
}
