use std::collections::BTreeMap;

use crate::*;

fn tree(files: &[(&str, &str)]) -> MapTree {
    MapTree(
        files
            .iter()
            .map(|(p, c)| (p.to_string(), c.to_string()))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn detect(files: &[(&str, &str)]) -> Plan {
    plan(&tree(files), &Overrides::default()).unwrap()
}

#[track_caller]
fn has(plan: &Plan, lines: &[&str]) {
    for line in lines {
        assert!(
            plan.dockerfile.contains(line),
            "no {line:?} in\n{}",
            plan.dockerfile
        );
    }
    assert!(
        plan.dockerfile
            .starts_with("# syntax=docker/dockerfile:1\n")
    );
}

fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

#[test]
fn next_standalone_ships_only_the_traced_server() {
    let p = detect(&[
        ("package.json", r#"{"scripts": {"build": "next build", "start": "next start"}, "dependencies": {"next": "15.0.0", "react": "19.0.0"}}"#),
        ("package-lock.json", "{}"),
        ("next.config.mjs", "const config = {\n  output: 'standalone',\n};\nexport default config;\n"),
        ("public/logo.svg", "<svg/>"),
        ("app/page.tsx", ""),
    ]);
    assert_eq!(p.start, s("node server.js"));
    has(
        &p,
        &[
            "COPY --from=build --chown=node:node /app/.next/standalone /app",
            "COPY --from=build --chown=node:node /app/.next/static /app/.next/static",
            "COPY --from=build --chown=node:node /app/public /app/public",
            "ENV HOSTNAME=0.0.0.0",
            r#"CMD ["sh", "-c", "exec node server.js"]"#,
        ],
    );
    assert!(!p.dockerfile.contains("COPY --from=build --chown=node:node /app /app") && !p.dockerfile.contains("prune"), "{}", p.dockerfile);
}

#[test]
fn next_on_pnpm() {
    let p = detect(&[
        (
            "package.json",
            r#"{"packageManager": "pnpm@9.12.0", "engines": {"node": ">=20"},
                "scripts": {"build": "next build", "dev": "next dev"},
                "dependencies": {"next": "15.0.0", "react": "19.0.0"}}"#,
        ),
        ("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
        ("app/page.tsx", "export default function Page() {}"),
    ]);
    assert_eq!(p.provider, Provider::Node);
    assert_eq!(p.framework, s("next"));
    assert_eq!(p.language_version, s("22"));
    assert_eq!(p.install, s("pnpm install --frozen-lockfile"));
    assert_eq!(p.build, s("pnpm run build"));
    assert_eq!(p.start, s("node_modules/.bin/next start -p $PORT"));
    assert_eq!(p.port, 8080);
    has(
        &p,
        &[
            "FROM node:22-slim AS base",
            "corepack enable",
            "COPY package.json pnpm-lock.yaml ./",
            "RUN --mount=type=cache,target=/root/.local/share/pnpm/store pnpm install --frozen-lockfile",
            "RUN --mount=type=cache,target=/app/.next/cache pnpm run build",
            "COPY --from=build --chown=node:node /app /app",
            "ENV PORT=8080\nEXPOSE 8080\nUSER node\n",
            r#"CMD ["sh", "-c", "exec node_modules/.bin/next start -p $PORT"]"#,
        ],
    );
    assert!(
        p.notes
            .iter()
            .any(|n| n == "packageManager is pnpm@9.12.0: pnpm"),
        "{:?}",
        p.notes
    );
    assert!(
        p.notes.iter().any(|n| n == "next in dependencies: Next.js"),
        "{:?}",
        p.notes
    );
}

#[test]
fn vite_on_yarn_is_a_static_site() {
    let p = detect(&[
        (
            "package.json",
            r#"{"scripts": {"dev": "vite", "build": "vite build"}, "devDependencies": {"vite": "6.0.0"}}"#,
        ),
        ("yarn.lock", "# yarn lockfile v1\n"),
        (".nvmrc", "v20.11.0\n"),
        ("index.html", "<div id=app></div>"),
    ]);
    assert_eq!(p.provider, Provider::Static);
    assert_eq!(p.framework, s("vite"));
    assert_eq!(p.language_version, s("20"));
    assert_eq!(p.install, s("yarn install --frozen-lockfile"));
    assert_eq!(p.build, s("yarn run build"));
    assert_eq!(p.start, s("httpd -f -v -p $PORT -h /site"));
    has(
        &p,
        &[
            "FROM node:20-slim AS base",
            "--mount=type=cache,target=/usr/local/share/.cache/yarn yarn install --frozen-lockfile",
            "FROM busybox:1.37\nCOPY --from=build /app/dist /site",
            "USER 65534:65534",
        ],
    );
}

#[test]
fn express_on_npm_with_a_start_script() {
    let p = detect(&[
        (
            "package.json",
            r#"{"main": "server.js", "scripts": {"start": "node server.js"}, "dependencies": {"express": "^5"}}"#,
        ),
        ("package-lock.json", "{}"),
        ("server.js", "require('express')().listen(process.env.PORT)"),
    ]);
    assert_eq!(
        (p.provider, p.framework.as_deref()),
        (Provider::Node, Some("express"))
    );
    assert_eq!(p.install, s("npm ci"));
    assert_eq!(p.build, None);
    // A plain `node …` start script runs without npm in front of it.
    assert_eq!(p.start, s("node server.js"));
    has(
        &p,
        &[
            "COPY package.json package-lock.json ./",
            "--mount=type=cache,target=/root/.npm npm ci",
            "NODE_ENV=production",
        ],
    );
    assert!(!p.dockerfile.contains("corepack"));
    // The runtime image leaves devDependencies out...
    assert!(p.dockerfile.contains("npm prune --omit=dev"), "{}", p.dockerfile);
    // ...unless the app starts with one.
    let ts = detect(&[
        ("package.json", r#"{"scripts": {"start": "ts-node server.ts"}, "dependencies": {"express": "^5"}, "devDependencies": {"ts-node": "^10", "typescript": "^5"}}"#),
        ("package-lock.json", "{}"),
        ("server.ts", ""),
    ]);
    assert!(!ts.dockerfile.contains("prune"), "{}", ts.dockerfile);
    assert!(ts.notes.iter().any(|n| n.contains("ts-node, a devDependency")), "{:?}", ts.notes);

    // A prestart hook, or a script that needs a shell, keeps npm running it.
    for pkg in [
        r#"{"scripts": {"prestart": "node migrate.js", "start": "node server.js"}, "dependencies": {"express": "^5"}}"#,
        r#"{"scripts": {"start": "node server.js && echo done"}, "dependencies": {"express": "^5"}}"#,
    ] {
        let p = detect(&[("package.json", pkg), ("package-lock.json", "{}"), ("server.js", "")]);
        assert_eq!(p.start, s("npm run start"), "{pkg}");
    }
}

#[test]
fn django_on_uv() {
    let p = detect(&[
        (
            "pyproject.toml",
            "[project]\nname = \"site\"\nrequires-python = \">=3.10\"\ndependencies = [\"Django>=5.1\", \"psycopg[binary]\"]\n",
        ),
        ("uv.lock", "version = 1\n"),
        (".python-version", "3.11\n"),
        (
            "manage.py",
            "os.environ.setdefault('DJANGO_SETTINGS_MODULE', 'mysite.settings')\n",
        ),
        ("mysite/settings.py", ""),
        ("mysite/wsgi.py", "application = get_wsgi_application()"),
    ]);
    assert_eq!(
        (p.provider, p.framework.as_deref()),
        (Provider::Python, Some("django"))
    );
    assert_eq!(p.language_version, s("3.11"));
    assert_eq!(
        p.install,
        s("uv sync --frozen --no-dev && uv pip install gunicorn")
    );
    assert_eq!(p.start, s("gunicorn mysite.wsgi --bind 0.0.0.0:$PORT"));
    has(
        &p,
        &[
            "FROM python:3.11-slim AS build",
            "COPY pyproject.toml uv.lock .python-version ./",
            "--mount=type=cache,target=/root/.cache/uv uv sync --frozen --no-dev --no-install-project",
            "COPY . .\nRUN --mount=type=cache,target=/root/.cache/uv uv sync --frozen --no-dev && uv pip install gunicorn",
            "COPY --from=build /opt/venv /opt/venv",
            "USER app",
        ],
    );
}

#[test]
fn fastapi_on_pip_finds_the_app() {
    let p = detect(&[
        (
            "requirements.txt",
            "fastapi==0.115.0\npydantic>=2 # models\n",
        ),
        ("app/__init__.py", ""),
        (
            "app/main.py",
            "from fastapi import FastAPI\n\napi: FastAPI = FastAPI(title='x')\n",
        ),
    ]);
    assert_eq!(
        (p.provider, p.framework.as_deref()),
        (Provider::Python, Some("fastapi"))
    );
    assert_eq!(p.language_version, s("3.12"));
    assert_eq!(
        p.install,
        s("pip install -r requirements.txt && pip install uvicorn")
    );
    assert_eq!(
        p.start,
        s("uvicorn app.main:api --host 0.0.0.0 --port $PORT")
    );
    has(
        &p,
        &[
            "COPY requirements.txt ./\nRUN --mount=type=cache,target=/root/.cache/pip pip install -r requirements.txt && pip install uvicorn\nCOPY . .\n",
        ],
    );
}

#[test]
fn go_builds_the_one_cmd_on_distroless() {
    let files = [
        ("go.mod", "module example.com/svc\n\ngo 1.23.1\n"),
        ("go.sum", ""),
        ("cmd/api/main.go", "package main\nfunc main() {}\n"),
        ("internal/db/db.go", "package db\n"),
    ];
    let p = detect(&files);
    assert_eq!(p.provider, Provider::Go);
    assert_eq!(p.language_version, s("1.23"));
    assert_eq!(
        p.build,
        s("go build -trimpath -ldflags=\"-s -w\" -o /out/server ./cmd/api")
    );
    assert_eq!(p.start, s("/app/server"));
    has(
        &p,
        &[
            "FROM golang:1.23 AS build",
            "ENV CGO_ENABLED=0",
            "COPY go.mod go.sum ./\nRUN --mount=type=cache,target=/go/pkg/mod go mod download",
            "FROM gcr.io/distroless/static-debian12:nonroot",
            r#"CMD ["/app/server"]"#,
        ],
    );

    let mut two = files.to_vec();
    two.push(("cmd/worker/main.go", "package main\n"));
    let err = plan(&tree(&two), &Overrides::default()).unwrap_err();
    assert!(
        matches!(err, Error::Ambiguous(_)) && err.to_string().contains("api, worker"),
        "{err}"
    );
    let o = Overrides {
        build: s("go build -o /out/server ./cmd/worker"),
        ..Default::default()
    };
    let p = plan(&tree(&two), &o).unwrap();
    assert_eq!(p.start, s("/app/server"));
    has(
        &p,
        &[
            "go build -o /out/server ./cmd/worker",
            "FROM gcr.io/distroless/static-debian12:nonroot",
        ],
    );
}

#[test]
fn rust_workspace_with_one_binary() {
    let files = [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
        ),
        (
            "Cargo.lock",
            "[[package]]\nname = \"openssl-sys\"\nversion = \"0.9.0\"\n",
        ),
        (
            "crates/api/Cargo.toml",
            "[package]\nname = \"api\"\nedition = \"2024\"\n",
        ),
        ("crates/api/src/main.rs", "fn main() {}"),
        ("crates/core/Cargo.toml", "[package]\nname = \"core\"\n"),
        ("crates/core/src/lib.rs", ""),
    ];
    let p = detect(&files);
    assert_eq!(p.provider, Provider::Rust);
    assert_eq!(
        p.build,
        s("cargo build --release --locked -p api --bin api")
    );
    assert_eq!(p.start, s("api"));
    has(
        &p,
        &[
            "FROM rust:1-slim-bookworm AS build",
            "pkg-config libssl-dev",
            "--mount=type=cache,target=/usr/local/cargo/registry",
            "--mount=type=cache,target=/target cargo build --release --locked -p api --bin api && mkdir -p /out && cp /target/release/api /out/",
            "FROM debian:bookworm-slim",
            "ca-certificates libssl3",
            "COPY --from=build /out/api /usr/local/bin/api",
            r#"CMD ["sh", "-c", "exec api"]"#,
        ],
    );

    let mut two = files.to_vec();
    two.push(("crates/core/src/bin/migrate.rs", "fn main() {}"));
    let err = plan(&tree(&two), &Overrides::default()).unwrap_err();
    assert!(
        matches!(err, Error::Ambiguous(_)) && err.to_string().contains("api, migrate"),
        "{err}"
    );
}

#[test]
fn rails_with_postgres_and_assets() {
    let p = detect(&[
        (
            "Gemfile",
            "source \"https://rubygems.org\"\nruby file: \".ruby-version\"\ngem \"rails\", \"~> 8.0\"\ngem \"pg\"\n",
        ),
        (
            "Gemfile.lock",
            "GEM\n  specs:\n    pg (1.5.9)\n    propshaft (1.1.0)\n    rails (8.0.1)\n",
        ),
        (".ruby-version", "ruby-3.2.2\n"),
        ("bin/rails", "#!/usr/bin/env ruby"),
        ("config/application.rb", ""),
        ("config.ru", "run Rails.application"),
    ]);
    assert_eq!(
        (p.provider, p.framework.as_deref()),
        (Provider::Ruby, Some("rails"))
    );
    assert_eq!(p.language_version, s("3.2"));
    assert_eq!(p.install, s("bundle install"));
    assert_eq!(
        p.build,
        s("SECRET_KEY_BASE_DUMMY=1 bin/rails assets:precompile")
    );
    assert_eq!(p.start, s("bin/rails server -b 0.0.0.0 -p $PORT"));
    has(
        &p,
        &[
            "FROM ruby:3.2-slim AS build",
            "libpq-dev",
            "COPY Gemfile Gemfile.lock .ruby-version ./",
            "BUNDLE_DEPLOYMENT=1",
            "RUN SECRET_KEY_BASE_DUMMY=1 bin/rails assets:precompile",
            "libpq5",
            "COPY --from=build /usr/local/bundle /usr/local/bundle",
        ],
    );
}

#[test]
fn laravel_with_vite_assets() {
    let p = detect(&[
        (
            "composer.json",
            r#"{"require": {"php": "^8.2", "laravel/framework": "^11.0", "ext-gd": "*", "ext-mbstring": "*"}}"#,
        ),
        ("composer.lock", "{}"),
        ("artisan", "#!/usr/bin/env php"),
        ("public/index.php", "<?php"),
        (
            "package.json",
            r#"{"scripts": {"build": "vite build"}, "devDependencies": {"vite": "6"}}"#,
        ),
        ("package-lock.json", "{}"),
    ]);
    assert_eq!(
        (p.provider, p.framework.as_deref()),
        (Provider::Php, Some("laravel"))
    );
    assert_eq!(p.language_version, s("8.3"));
    assert_eq!(
        p.install,
        s("composer install --no-dev --optimize-autoloader --no-interaction")
    );
    assert_eq!(p.start, s("php -S 0.0.0.0:$PORT -t public"));
    has(
        &p,
        &[
            "FROM node:22-slim AS assets-base",
            "FROM assets-base AS assets",
            "npm run build && rm -rf node_modules",
            "FROM php:8.3-cli AS base",
            "RUN install-php-extensions gd pdo_mysql pdo_pgsql",
            "COPY composer.json composer.lock ./",
            "COPY --from=assets /app ./\nRUN --mount=type=cache,target=/tmp/composer composer dump-autoload --optimize --no-dev",
            "USER www-data",
        ],
    );
}

#[test]
fn spring_boot_on_maven() {
    let p = detect(&[
        (
            "pom.xml",
            "<project><parent><artifactId>spring-boot-starter-parent</artifactId></parent>\
             <properties><java.version>17</java.version></properties></project>",
        ),
        ("mvnw", "#!/bin/sh"),
        ("src/main/java/App.java", "class App {}"),
    ]);
    assert_eq!(
        (p.provider, p.framework.as_deref()),
        (Provider::Java, Some("spring"))
    );
    assert_eq!(p.language_version, s("17"));
    assert_eq!(p.build, s("./mvnw -B -DskipTests package"));
    assert_eq!(
        p.start,
        s("java $JAVA_OPTS -Dserver.port=$PORT -jar /app/app.jar")
    );
    has(
        &p,
        &[
            "FROM eclipse-temurin:17-jdk AS build",
            "RUN chmod +x mvnw",
            "--mount=type=cache,target=/root/.m2 ./mvnw -B -DskipTests package && mkdir -p /out",
            "FROM eclipse-temurin:17-jre",
            "COPY --from=build /out/app.jar /app/app.jar",
        ],
    );
}

#[test]
fn plain_index_html_and_nothing() {
    let p = detect(&[("index.html", "<h1>hi</h1>"), ("style.css", "")]);
    assert_eq!(p.provider, Provider::Static);
    assert_eq!(p.start, s("httpd -f -v -p $PORT -h /site"));
    has(
        &p,
        &[
            "FROM busybox:1.37\nCOPY . /site\n",
            r#"CMD ["sh", "-c", "exec httpd -f -v -p $PORT -h /site"]"#,
        ],
    );

    let err = plan(&tree(&[("README.md", "hi")]), &Overrides::default()).unwrap_err();
    assert!(matches!(err, Error::NothingDetected));
    assert!(
        err.to_string().contains("package.json") && err.to_string().contains("go.mod"),
        "{err}"
    );
}

#[test]
fn a_dockerfile_wins_and_can_be_chosen() {
    let files = [
        (
            "Dockerfile",
            "FROM node:22\nEXPOSE 9000\nEXPOSE 3000/tcp\nCMD node .\n",
        ),
        ("docker/web.Dockerfile", "FROM nginx\n"),
        ("package.json", "{}"),
    ];
    let p = detect(&files);
    assert_eq!(p.provider, Provider::Dockerfile);
    assert_eq!(p.port, 3000);
    assert_eq!(p.dockerfile, files[0].1);
    assert_eq!((p.install, p.build, p.start), (None, None, None));

    let pick = |path: &str| {
        let o = Overrides {
            builder: Builder::Dockerfile,
            dockerfile: s(path),
            ..Default::default()
        };
        plan(&tree(&files), &o)
    };
    let p = pick("./docker/web.Dockerfile").unwrap();
    assert_eq!((p.port, p.dockerfile.as_str()), (8080, "FROM nginx\n"));
    assert!(matches!(
        pick("docker/missing"),
        Err(Error::NoDockerfile(_))
    ));
    assert!(matches!(
        pick("../Dockerfile"),
        Err(Error::BadDockerfilePath(_))
    ));
    assert!(matches!(
        pick("/etc/passwd"),
        Err(Error::BadDockerfilePath(_))
    ));
    let o = Overrides {
        builder: Builder::Dockerfile,
        ..Default::default()
    };
    assert!(matches!(
        plan(&tree(&[("index.html", "")]), &o),
        Err(Error::NoDockerfile(_))
    ));

    let o: Overrides =
        serde_json::from_str(r#"{"builder": "dockerfile", "dockerfile": "docker/web.Dockerfile"}"#)
            .unwrap();
    assert_eq!(o.builder, Builder::Dockerfile);
}

#[test]
fn overrides_replace_the_detected_commands() {
    let files = [
        (
            "package.json",
            r#"{"scripts": {"start": "node server.js"}, "dependencies": {"express": "5"}}"#,
        ),
        ("package-lock.json", "{}"),
        ("server.js", ""),
    ];
    let o = Overrides {
        install: s("npm ci --omit=optional"),
        build: s("npm run compile\nnpm run bundle"),
        start: s("  node dist/server.js --port $PORT "),
        ..Default::default()
    };
    let p = plan(&tree(&files), &o).unwrap();
    assert_eq!(p.install, s("npm ci --omit=optional"));
    assert_eq!(p.build, s("npm run compile\nnpm run bundle"));
    assert_eq!(p.start, s("node dist/server.js --port $PORT"));
    has(
        &p,
        &[
            // A custom install may read anything, so the whole source is there first.
            "COPY . .\nRUN --mount=type=cache,target=/root/.npm npm ci --omit=optional",
            r#"RUN ["/bin/sh", "-c", "npm run compile\nnpm run bundle"]"#,
            r#"CMD ["sh", "-c", "exec node dist/server.js --port $PORT"]"#,
        ],
    );
    assert!(!p.dockerfile.contains("COPY package.json"));
    assert!(
        p.notes
            .iter()
            .any(|n| n == "start is set in the service: node dist/server.js --port $PORT")
    );

    // Blank overrides count as unset.
    let o = Overrides {
        start: s("  "),
        ..Default::default()
    };
    assert_eq!(plan(&tree(&files), &o).unwrap().start, s("node server.js"));
}

#[cfg(unix)]
#[test]
fn dir_tree_refuses_symlinks_and_escapes() {
    use std::os::unix::fs::symlink;

    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "token").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("src");
    std::fs::create_dir_all(root.join("app")).unwrap();
    std::fs::write(root.join("app/main.py"), "print(1)").unwrap();
    std::fs::write(root.join("big"), vec![b'x'; 3 << 20]).unwrap();
    symlink(outside.path().join("secret"), root.join("requirements.txt")).unwrap();
    symlink(outside.path(), root.join("linked")).unwrap();
    std::fs::write(dir.path().join("sibling"), "no").unwrap();

    let t = DirTree::new(&root);
    assert_eq!(t.read("app/main.py").as_deref(), Some(&b"print(1)"[..]));
    assert_eq!(t.read("big").map(|b| b.len()), Some(1 << 20));
    assert_eq!(t.read("requirements.txt"), None);
    assert!(!t.exists("requirements.txt"));
    assert_eq!(t.read("linked/secret"), None);
    assert!(t.list("linked").is_empty());
    assert_eq!(t.read("../sibling"), None);
    assert_eq!(t.read("app/../../sibling"), None);
    assert_eq!(t.read(&dir.path().join("sibling").to_string_lossy()), None);
    assert_eq!(t.list(""), ["app/", "big"]);
    // The symlinked requirements.txt doesn't count, so this is just a file with no manifest.
    assert!(matches!(
        plan(&t, &Overrides::default()),
        Err(Error::NothingDetected)
    ));
}
