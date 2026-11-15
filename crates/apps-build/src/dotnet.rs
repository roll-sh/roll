//! .NET: `dotnet publish` on the SDK image, run on the ASP.NET runtime image (Heroku's and
//! Railpack's .NET), listening on `$PORT`.

use crate::df::Df;
use crate::{Commands, Draft, Error, Overrides, Provider, Tree, text};

pub(crate) fn detect(tree: &dyn Tree, _o: &Overrides) -> Result<Option<Draft>, Error> {
    let projects: Vec<String> = tree.list("").into_iter().filter(|f| f.ends_with(".csproj") || f.ends_with(".fsproj")).collect();
    let project = match projects.as_slice() {
        [] => return Ok(None),
        [one] => one.clone(),
        many => return Err(Error::Ambiguous(format!("{} projects at the root ({}): set the root directory to one, or a Dockerfile", many.len(), many.join(", ")))),
    };
    let xml = text(tree, &project).unwrap_or_default();
    let tag = |name: &str| -> Option<String> {
        let open = format!("<{name}>");
        let i = xml.find(&open)? + open.len();
        let j = xml[i..].find('<')? + i;
        Some(xml[i..j].trim().to_string()).filter(|s| !s.is_empty())
    };
    let mut notes = vec![format!("found {project}: .NET")];
    // net8.0 → 8.0; a multi-target project builds its first.
    let framework_tag = tag("TargetFramework").or_else(|| tag("TargetFrameworks").and_then(|t| t.split(';').next().map(str::to_string)));
    let version = match framework_tag.as_deref().and_then(|t| t.strip_prefix("net")).filter(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit())) {
        Some(v) => {
            notes.push(format!("TargetFramework {}: .NET {v}", framework_tag.as_deref().unwrap_or("")));
            v.to_string()
        }
        None => {
            notes.push("no TargetFramework read: .NET 8.0".into());
            "8.0".into()
        }
    };
    let stem = project.rsplit_once('.').map_or(project.as_str(), |(s, _)| s).to_string();
    let assembly = tag("AssemblyName").unwrap_or(stem);
    let web = xml.contains("Microsoft.NET.Sdk.Web");
    let start = format!("dotnet /app/{assembly}.dll --urls http://0.0.0.0:$PORT");
    notes.push(format!("published as {assembly}.dll, listening on $PORT"));
    let manifests = vec![project.clone()];
    let render = Box::new(move |c: &Commands<'_>| {
        let mut df = Df::new(c);
        df.from(&format!("mcr.microsoft.com/dotnet/sdk:{version}"), Some("build"));
        df.line("WORKDIR /src");
        // The packages are a cache restore and publish both read: they're in the published output.
        let caches = ["/root/.nuget/packages"];
        df.source(Some(&manifests), &caches, c);
        if let Some(build) = c.build {
            df.run(&caches, build);
        }
        df.from(&format!("mcr.microsoft.com/dotnet/aspnet:{version}"), None);
        df.line("WORKDIR /app");
        df.line("COPY --from=build /out ./");
        // The images' own non-root user.
        df.serve(Some("app"), c.start);
        df.finish()
    });
    Ok(Some(Draft {
        provider: Provider::Dotnet,
        framework: web.then_some("aspnetcore"),
        language_version: Some(version_major(&framework_tag)),
        install: Some("dotnet restore".into()),
        build: Some("dotnet publish -c Release -o /out --no-restore".into()),
        start: Some(start),
        notes,
        render,
    }))
}

fn version_major(t: &Option<String>) -> String {
    t.as_deref().and_then(|t| t.strip_prefix("net")).unwrap_or("8.0").to_string()
}
