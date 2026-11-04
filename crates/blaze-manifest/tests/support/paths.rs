use std::path::Path;

use serde_json::Value;

pub fn normalize(text: &str, root: &Path, home: &Path) -> String {
    let mut text = text.to_owned();
    for (path, marker) in [(home, "$CARGO_HOME"), (root, "$ROOT")] {
        let native = path.to_str().unwrap();
        let url = url::Url::from_file_path(path).unwrap().to_string();
        let escaped = serde_json::to_string(native).unwrap();
        let escaped = &escaped[1..escaped.len() - 1];
        text = text.replace(&format!("'{native}'"), marker);
        let windows = native.contains('\\');
        for (prefix, separator) in [
            (escaped, windows.then_some("\\\\")),
            (native, windows.then_some("\\")),
            (url.strip_prefix("file://").unwrap(), None),
        ] {
            let mut normalized = String::new();
            let mut rest = text.as_str();
            while let Some((before, after)) = rest.split_once(prefix) {
                normalized.push_str(before);
                normalized.push_str(marker);
                let mut suffix = after;
                loop {
                    if let Some(tail) = separator.and_then(|sep| suffix.strip_prefix(sep)) {
                        normalized.push('/');
                        suffix = tail;
                    } else if let Some(c) = suffix
                        .chars()
                        .next()
                        .filter(|c| c.is_alphanumeric() || matches!(c, '/' | '_' | '-' | '.'))
                    {
                        normalized.push(c);
                        suffix = &suffix[c.len_utf8()..];
                    } else {
                        break;
                    }
                }
                rest = suffix;
            }
            normalized.push_str(rest);
            text = normalized;
        }
    }
    text
}

/// Fixture paths use portable slash-delimited components. Emit native paths,
/// including on Windows, without altering file URLs or diagnostic text.
fn native_fixture_path(text: &str, marker: &str, root: &Path) -> Option<String> {
    let suffix = text.strip_prefix(marker)?;
    if (!suffix.is_empty() && !suffix.starts_with('/')) || suffix.chars().any(char::is_whitespace) {
        return None;
    }
    let mut path = root.to_path_buf();
    for component in suffix.split('/').filter(|part| !part.is_empty()) {
        path.push(component);
    }
    Some(path.to_string_lossy().into_owned())
}

pub fn expand(value: &mut Value, root: &Path, home: &Path) {
    match value {
        Value::String(text) => {
            for (path, marker) in [(root, "$ROOT"), (home, "$CARGO_HOME")] {
                let expanded_url = text.replace(
                    &format!("file://{marker}"),
                    url::Url::from_file_path(path).unwrap().as_str(),
                );
                *text = native_fixture_path(&expanded_url, marker, path)
                    .unwrap_or_else(|| expanded_url.replace(marker, path.to_str().unwrap()));
            }
        }
        Value::Array(values) => values.iter_mut().for_each(|v| expand(v, root, home)),
        Value::Object(values) => values.values_mut().for_each(|v| expand(v, root, home)),
        _ => {}
    }
}

#[test]
fn fixture_paths_preserve_unrelated_escapes_and_json() {
    let dir = std::env::temp_dir().join("fixture-paths");
    let home = dir.join("home");
    let root = dir.join("root");
    let path = root.join("src/lib.rs");
    let text = format!("{} literal \\n \\other\\value", path.display());
    assert_eq!(normalize(&text, &root, &home), "$ROOT/src/lib.rs literal \\n \\other\\value");
    let mut value = serde_json::json!({"path": "$ROOT/src/lib.rs", "id": "path+file://$ROOT#0.1.0", "error": "$ROOT/src/lib.rs\nnext line"});
    expand(&mut value, &root, &home);
    let encoded = serde_json::to_string(&value).unwrap();
    let normalized = normalize(&encoded, &root, &home);
    assert_eq!(
        serde_json::from_str::<Value>(&normalized).unwrap(),
        serde_json::json!({
            "path": "$ROOT/src/lib.rs", "id": "path+file://$ROOT#0.1.0", "error": "$ROOT/src/lib.rs\nnext line"
        })
    );
}

#[test]
fn fixture_expansion_uses_native_components_and_preserves_urls_and_diagnostics() {
    let dir = std::env::temp_dir().join("fixture-native-components");
    let home = dir.join("home");
    let root = dir.join("root");
    let mut value = serde_json::json!({
        "src_path": "$ROOT/crates/app/src/main.rs",
        "dependency_path": "$ROOT/tools/helper",
        "home_path": "$CARGO_HOME/registry/cache",
        "root_path": "$ROOT",
        "id": "path+file://$ROOT/crates/app#0.3.0",
        "error": "$ROOT/src/lib.rs\nnext line"
    });
    expand(&mut value, &root, &home);
    assert_eq!(
        value["src_path"].as_str().unwrap(),
        root.join("crates").join("app").join("src").join("main.rs").to_str().unwrap()
    );
    assert_eq!(
        value["dependency_path"].as_str().unwrap(),
        root.join("tools").join("helper").to_str().unwrap()
    );
    assert_eq!(
        value["home_path"].as_str().unwrap(),
        home.join("registry").join("cache").to_str().unwrap()
    );
    assert_eq!(value["root_path"].as_str().unwrap(), root.to_str().unwrap());
    assert_eq!(
        value["id"].as_str().unwrap(),
        format!("path+{}/crates/app#0.3.0", url::Url::from_file_path(&root).unwrap())
    );
    assert_eq!(
        value["error"].as_str().unwrap(),
        format!("{}/src/lib.rs\nnext line", root.display())
    );
}
