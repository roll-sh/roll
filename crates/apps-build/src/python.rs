//! Python: uv, poetry and pip; Django, FastAPI and Flask. Dependencies go in a virtualenv at
//! `/opt/venv`, which the runtime stage copies without the build's caches and tools.

use std::collections::BTreeSet;

use crate::df::Df;
use crate::version::{self, Ver};
use crate::{Commands, Draft, Error, Overrides, Provider, Tree, invalid, join, node, text};

const DEFAULT: Ver = Ver(3, 12);
const UV_IMAGE: &str = "ghcr.io/astral-sh/uv:latest";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Manager {
    Uv,
    Poetry,
    Requirements,
    /// A PEP 621 `[project]`: `pip install .`.
    Project,
}

pub(crate) fn detect(tree: &dyn Tree, _: &Overrides) -> Result<Option<Draft>, Error> {
    let requirements = text(tree, "requirements.txt");
    let pyproject = match text(tree, "pyproject.toml") {
        Some(t) => {
            Some(toml::from_str::<toml::Table>(&t).map_err(|e| invalid("pyproject.toml", e))?)
        }
        None => None,
    };
    if requirements.is_none() && pyproject.is_none() {
        return Ok(None);
    }
    let mut notes = Vec::new();
    let table = |path: &[&str]| -> Option<&toml::Value> {
        let mut v = pyproject.as_ref()?.get(path[0])?;
        for key in &path[1..] {
            v = v.get(key)?;
        }
        Some(v)
    };
    let manager = if tree.exists("uv.lock") {
        notes.push("found uv.lock: uv".into());
        Manager::Uv
    } else if tree.exists("poetry.lock") || table(&["tool", "poetry"]).is_some() {
        notes.push(
            if tree.exists("poetry.lock") {
                "found poetry.lock: poetry"
            } else {
                "[tool.poetry] in pyproject.toml: poetry"
            }
            .into(),
        );
        Manager::Poetry
    } else if requirements.is_some() {
        notes.push("found requirements.txt: pip".into());
        Manager::Requirements
    } else {
        notes.push("found pyproject.toml: pip install .".into());
        Manager::Project
    };

    // Dependency names, normalized ("Flask_SQLAlchemy" is "flask-sqlalchemy"), and their specs.
    let mut specs: Vec<String> = Vec::new();
    if let Some(r) = &requirements {
        specs.extend(
            r.lines()
                .map(|l| l.split('#').next().unwrap_or("").trim().to_string()),
        );
    }
    if let Some(deps) = table(&["project", "dependencies"]).and_then(|d| d.as_array()) {
        specs.extend(deps.iter().filter_map(|d| d.as_str()).map(String::from));
    }
    if let Some(deps) = table(&["tool", "poetry", "dependencies"]).and_then(|d| d.as_table()) {
        specs.extend(deps.keys().cloned());
    }
    let names: BTreeSet<String> = specs.iter().filter_map(|s| name(s)).collect();
    let has = |n: &str| names.contains(n);

    let version = python_version(
        tree,
        table(&["project", "requires-python"]).and_then(|v| v.as_str()),
        &mut notes,
    );

    let mut extra: Option<&str> = None;
    let start = if tree.exists("manage.py")
        && (has("django") || text(tree, "manage.py").is_some_and(|m| m.contains("django")))
    {
        notes.push("found manage.py: Django".into());
        match wsgi_module(tree) {
            Some(module) => {
                notes.push(format!(
                    "found {}/wsgi.py: gunicorn serves {module}",
                    module.trim_end_matches(".wsgi").replace('.', "/")
                ));
                if !has("gunicorn") {
                    extra = Some("gunicorn");
                }
                Some(format!("gunicorn {module} --bind 0.0.0.0:$PORT"))
            }
            None => {
                notes.push("no <project>/wsgi.py found: set start".into());
                None
            }
        }
        .map(|s| (Some("django"), s))
    } else if has("fastapi") {
        notes.push("fastapi in dependencies: FastAPI".into());
        let app = find_app(tree, "FastAPI(").unwrap_or_else(|| {
            notes.push("no `= FastAPI(` found: assuming main:app".into());
            "main:app".into()
        });
        let standard = specs.iter().any(|s| {
            s.replace(' ', "")
                .to_lowercase()
                .starts_with("fastapi[standard")
                || s.contains("fastapi[all")
        });
        if !has("uvicorn") && !standard {
            extra = Some("uvicorn");
        }
        Some((
            Some("fastapi"),
            format!("uvicorn {app} --host 0.0.0.0 --port $PORT"),
        ))
    } else if has("flask") {
        notes.push("flask in dependencies: Flask".into());
        let app = find_app(tree, "Flask(").unwrap_or_else(|| {
            notes.push("no `= Flask(` found: assuming app:app".into());
            "app:app".into()
        });
        if !has("gunicorn") {
            extra = Some("gunicorn");
        }
        Some((
            Some("flask"),
            format!("gunicorn --bind 0.0.0.0:$PORT '{app}'"),
        ))
    } else {
        ["main.py", "app.py", "server.py"]
            .into_iter()
            .find(|f| tree.read(f).is_some())
            .map(|f| (None, format!("python {f}")))
    };
    let (framework, start) = match start {
        Some((fw, s)) => (fw, Some(s)),
        None => (None, None),
    };
    if let Some(e) = extra {
        notes.push(format!("{e} isn't in the dependencies: installing it"));
    }
    match &start {
        Some(s) => notes.push(format!("starts with {s}")),
        None => notes.push("nothing says how to start it: add main.py or set start".into()),
    }

    let whole_tree = requirements.as_deref().is_some_and(|r| {
        r.lines().any(|l| {
            let l = l.trim_start();
            [
                "-r",
                "-c",
                "-e",
                "--requirement",
                "--constraint",
                "--editable",
                ".",
            ]
            .iter()
            .any(|p| l.starts_with(p))
        })
    });
    let present = |files: &[&str]| -> Vec<String> {
        files
            .iter()
            .filter(|f| tree.exists(f))
            .map(|f| f.to_string())
            .collect()
    };
    let (pre, post, manifests, cache) = match manager {
        Manager::Uv => (
            "uv sync --frozen --no-dev --no-install-project",
            "uv sync --frozen --no-dev",
            Some(present(&["pyproject.toml", "uv.lock", ".python-version"])),
            "/root/.cache/uv",
        ),
        Manager::Poetry => (
            "poetry install --only main --no-root --no-interaction",
            "",
            Some(present(&["pyproject.toml", "poetry.lock"])),
            "/root/.cache/pypoetry",
        ),
        Manager::Requirements => (
            "pip install -r requirements.txt",
            "",
            (!whole_tree).then(|| vec!["requirements.txt".to_string()]),
            "/root/.cache/pip",
        ),
        Manager::Project => ("", "pip install .", None, "/root/.cache/pip"),
    };
    let add = extra.map(|e| {
        if manager == Manager::Uv {
            format!("uv pip install {e}")
        } else {
            format!("pip install {e}")
        }
    });
    let chain = |cmds: &[&str]| -> String {
        cmds.iter()
            .copied()
            .filter(|s| !s.is_empty())
            .chain(add.as_deref())
            .collect::<Vec<_>>()
            .join(" && ")
    };
    // With the manifests copied first: `first` runs before the source is copied, `then` after.
    // uv's first sync only fills the dependency layer, and its last one removes anything else.
    let (first, then) = match manager {
        Manager::Uv => (pre.to_string(), chain(&[post])),
        _ => (chain(&[pre]), String::new()),
    };
    let install = chain(&[if manager == Manager::Uv { "" } else { pre }, post]);
    let install = (!install.is_empty()).then_some(install);

    let assets = node::assets(tree, &mut notes)?;
    let image = format!("python:{}-slim", version.minor());
    let render = Box::new(move |c: &Commands<'_>| {
        let mut df = Df::with_assets(assets.as_ref(), c);
        df.from(&image, Some("build"));
        df.env(&[
            ("PYTHONDONTWRITEBYTECODE", "1"),
            ("PIP_DISABLE_PIP_VERSION_CHECK", "1"),
        ]);
        match manager {
            Manager::Uv => {
                df.line(&format!("COPY --from={UV_IMAGE} /uv /usr/local/bin/uv"));
                df.env(&[
                    ("UV_PROJECT_ENVIRONMENT", "/opt/venv"),
                    ("UV_LINK_MODE", "copy"),
                    ("UV_COMPILE_BYTECODE", "1"),
                    ("UV_PYTHON_DOWNLOADS", "never"),
                ]);
            }
            Manager::Poetry => {
                df.run(&["/root/.cache/pip"], "pip install poetry");
                df.env(&[("POETRY_VIRTUALENVS_CREATE", "false")]);
                df.line("RUN python -m venv /opt/venv");
            }
            _ => df.line("RUN python -m venv /opt/venv"),
        }
        df.env(&[
            ("VIRTUAL_ENV", "/opt/venv"),
            ("PATH", "/opt/venv/bin:$PATH"),
        ]);
        df.line("WORKDIR /app");
        let caches = [cache];
        match (c.custom_install, &manifests) {
            (false, Some(files)) if !files.is_empty() && !pre.is_empty() => {
                df.line(&format!("COPY {} ./", files.join(" ")));
                df.run(&caches, &first);
                df.copy_source();
                if !then.is_empty() {
                    df.run(&caches, &then);
                }
            }
            _ => {
                df.copy_source();
                if let Some(install) = c.install {
                    df.run(&caches, install);
                }
            }
        }
        if let Some(build) = c.build {
            df.run(&caches, build);
        }
        df.from(&image, None);
        df.env(&[
            ("PYTHONUNBUFFERED", "1"),
            ("VIRTUAL_ENV", "/opt/venv"),
            ("PATH", "/opt/venv/bin:$PATH"),
        ]);
        df.line("RUN useradd --create-home --uid 10001 app");
        df.line("WORKDIR /app");
        df.line("COPY --from=build /opt/venv /opt/venv");
        df.line("COPY --from=build --chown=app:app /app /app");
        df.serve(Some("app"), c.start);
        df.finish()
    });
    Ok(Some(Draft {
        provider: Provider::Python,
        framework,
        language_version: Some(version.minor()),
        install,
        build: None,
        start,
        notes,
        render,
    }))
}

/// The normalized name of a requirement (`Django>=5`, `uvicorn[standard]`, `flask ; python_version…`).
fn name(spec: &str) -> Option<String> {
    let spec = spec.trim();
    if spec.starts_with('-') {
        return None;
    }
    let end = spec
        .find(|c: char| !(c.is_ascii_alphanumeric() || "._-".contains(c)))
        .unwrap_or(spec.len());
    let n = &spec[..end];
    (!n.is_empty()).then(|| n.to_ascii_lowercase().replace(['_', '.'], "-"))
}

fn python_version(tree: &dyn Tree, requires: Option<&str>, notes: &mut Vec<String>) -> Ver {
    if let Some(pin) = text(tree, ".python-version")
        && let Some(v) = pin.lines().next().and_then(version::parse)
    {
        notes.push(format!(
            ".python-version pins {}: Python {}",
            pin.lines().next().unwrap_or("").trim(),
            v.minor()
        ));
        return v;
    }
    if let Some(c) = requires
        && let Some(v) = version::pick(c, DEFAULT)
    {
        notes.push(format!("requires-python is {c}: Python {}", v.minor()));
        return v;
    }
    if let Some(rt) = text(tree, "runtime.txt")
        && let Some(v) = version::parse(rt.trim())
    {
        notes.push(format!(
            "runtime.txt says {}: Python {}",
            rt.trim(),
            v.minor()
        ));
        return v;
    }
    notes.push(format!("no Python version set: Python {}", DEFAULT.minor()));
    DEFAULT
}

/// `<project>.wsgi`: from manage.py's settings module, else the one `*/wsgi.py`.
fn wsgi_module(tree: &dyn Tree) -> Option<String> {
    let manage = text(tree, "manage.py").unwrap_or_default();
    let settings = manage
        .split("DJANGO_SETTINGS_MODULE")
        .nth(1)
        .and_then(|rest| {
            let rest = rest.trim_start_matches(|c: char| c != '"' && c != '\'');
            let quote = rest.chars().next()?;
            rest[1..].split(quote).next().map(String::from)
        });
    if let Some(project) = settings.as_deref().and_then(|s| s.split('.').next())
        && tree.read(&format!("{project}/wsgi.py")).is_some()
    {
        return Some(format!("{project}.wsgi"));
    }
    let found: Vec<String> = tree
        .list("")
        .into_iter()
        .filter_map(|d| d.strip_suffix('/').map(String::from))
        .filter(|d| tree.read(&format!("{d}/wsgi.py")).is_some())
        .collect();
    match found.as_slice() {
        [one] => Some(format!("{one}.wsgi")),
        _ => None,
    }
}

/// `module:variable` for the first top-level `variable = <ctor>…` (or a `create_app` factory)
/// in the usual places.
fn find_app(tree: &dyn Tree, ctor: &str) -> Option<String> {
    let mut files: Vec<String> = [
        "main.py",
        "app.py",
        "server.py",
        "api.py",
        "wsgi.py",
        "asgi.py",
    ]
    .map(String::from)
    .into();
    for dir in ["app", "src", "api", "src/app"] {
        files.extend(["main.py", "__init__.py", "app.py", "server.py"].map(|f| join(dir, f)));
    }
    files.extend(tree.list("").into_iter().filter(|f| f.ends_with(".py")));
    let mut factory = None;
    for file in &files {
        let Some(src) = text(tree, file) else {
            continue;
        };
        let module = file
            .trim_end_matches(".py")
            .trim_end_matches("/__init__")
            .replace('/', ".");
        for line in src.lines() {
            if let Some((var, value)) = line.split_once('=')
                && !line.starts_with([' ', '\t'])
                && value.trim_start().starts_with(ctor)
            {
                let var = var
                    .trim()
                    .trim_end_matches(|c: char| c == ':' || c.is_whitespace());
                let var = var.split(':').next().unwrap_or(var).trim();
                if !var.is_empty() && var.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    return Some(format!("{module}:{var}"));
                }
            }
            if factory.is_none() && line.starts_with("def create_app(") {
                factory = Some(format!("{module}:create_app()"));
            }
        }
    }
    factory
}
