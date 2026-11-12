//! The tree's own Dockerfile.

use crate::tree::valid;
use crate::{DEFAULT_PORT, Error, Overrides, Plan, Provider, Tree, text};

pub(crate) fn plan(tree: &dyn Tree, o: &Overrides) -> Result<Plan, Error> {
    let path = o.dockerfile.as_deref().unwrap_or("Dockerfile");
    let path = path.strip_prefix("./").unwrap_or(path);
    if !valid(path) {
        return Err(Error::BadDockerfilePath(path.to_string()));
    }
    let Some(dockerfile) = text(tree, path) else {
        return Err(Error::NoDockerfile(path.to_string()));
    };
    let mut notes = vec![format!("found {path}: building it as it is")];
    let port = match expose(&dockerfile) {
        Some(port) => {
            notes.push(format!("{path} exposes {port}: the app listens there"));
            port
        }
        None => {
            notes.push(format!(
                "{path} has no EXPOSE: the app should listen on $PORT ({DEFAULT_PORT})"
            ));
            DEFAULT_PORT
        }
    };
    for (what, set) in [("install", &o.install), ("build", &o.build)] {
        if set.is_some() {
            notes.push(format!(
                "the {what} command is ignored: {path} decides how to build"
            ));
        }
    }
    if let Some(start) = &o.start {
        notes.push(format!(
            "start is set in the service: {start} replaces the image's command"
        ));
    }
    Ok(Plan {
        provider: Provider::Dockerfile,
        framework: None,
        language_version: None,
        install: None,
        build: None,
        start: o.start.clone(),
        port,
        dockerfile,
        notes,
    })
}

/// The first port of the last `EXPOSE` (`EXPOSE 3000/tcp`); `None` for `EXPOSE $PORT`.
fn expose(dockerfile: &str) -> Option<u16> {
    let mut port = None;
    let joined = dockerfile.replace("\\\r\n", " ").replace("\\\n", " ");
    for line in joined.lines() {
        let mut words = line.split_whitespace();
        if !words
            .next()
            .is_some_and(|w| w.eq_ignore_ascii_case("EXPOSE"))
        {
            continue;
        }
        port = words
            .next()
            .and_then(|w| w.split('/').next()?.parse::<u16>().ok())
            .filter(|p| *p > 0);
    }
    port
}
