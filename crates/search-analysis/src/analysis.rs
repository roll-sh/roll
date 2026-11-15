//! One index's analysis configuration (`index.analysis`), built with Elasticsearch's rules.
//!
//! Building mirrors `AnalysisRegistry.build`: configured char filters, tokenizers and filters are
//! constructed first (so their validation errors come first), then analyzers and normalizers,
//! then the implicit `default` analyzer. A name inside an analyzer definition resolves to a
//! configured component, then a pre-configured one, then a component type built with default
//! settings. `_analyze` with inline components resolves differently (see [`Scope`]), as
//! Elasticsearch does.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};

use crate::analyzer::{Analyzer, AnalyzerSpec, Normalizer};
use crate::analyzers::{self, POSITION_INCREMENT_GAP};
use crate::component::{
    ChainParts, CharFilterFactory, FilterFactory, NamedCharFilter, NamedFilter, NamedTokenizer,
    SynonymRole, TokenizerFactory,
};
use crate::error::{AnalysisError, Result};
use crate::registry::{self, Ctx};
use crate::settings::Settings;
use crate::{IndexOptions, tokenizer};

/// An index's analyzers, normalizers and analysis components. Cheap to clone.
#[derive(Clone)]
pub struct Analysis {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub opts: IndexOptions,
    /// No index: `_analyze` without an index resolves names globally.
    pub global: bool,
    pub settings: Settings,
    pub char_filters: BTreeMap<String, NamedCharFilter>,
    pub tokenizers: BTreeMap<String, NamedTokenizer>,
    pub filters: BTreeMap<String, NamedFilter>,
    pub analyzers: HashMap<String, Analyzer>,
    pub normalizers: HashMap<String, Normalizer>,
    pub whitespace_normalizers: HashMap<String, Normalizer>,
}

/// The kinds of components, with the names Elasticsearch uses in messages.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    CharFilter,
    Tokenizer,
    Filter,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::CharFilter => "char_filter",
            Kind::Tokenizer => "tokenizer",
            Kind::Filter => "filter",
        }
    }
}

/// How a component name is resolved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Scope {
    /// Inside index analysis definitions: configured, pre-configured, then default instances.
    Index,
    /// `_analyze` on an index: configured, then any type by name with empty settings, then
    /// pre-configured.
    AdHocIndex,
    /// `_analyze` without an index: pre-configured, then types that need no settings.
    AdHocGlobal,
}

/// A resolved component of any kind.
pub(crate) enum Component {
    CharFilter(NamedCharFilter),
    Tokenizer(NamedTokenizer),
    Filter(NamedFilter),
}

/// The iteration order of a Java `HashMap` holding `names`, inserted in that order: by bucket
/// (Java's string hash, spread, masked by the table size the map ends up with), then by
/// insertion within a bucket.
fn java_hashmap_order(names: Vec<String>) -> Vec<String> {
    let mut cap = 16usize;
    while names.len() > cap * 3 / 4 {
        cap *= 2;
    }
    let mut v: Vec<(u32, usize, String)> = names
        .into_iter()
        .enumerate()
        .map(|(i, n)| (crate::tokenizer::java_bucket(&n, cap as u32), i, n))
        .collect();
    v.sort();
    v.into_iter().map(|(_, _, n)| n).collect()
}

/// `Settings.getGroups(prefix)` in its iteration order (a `HashMap` filled from a `HashSet` of
/// the group names, i.e. bucket order, sorted names within a bucket).
fn settings_groups(settings: &Settings, prefix: &str) -> Vec<(String, Settings)> {
    let mut groups = settings.groups(prefix);
    let order = java_hashmap_order(groups.keys().cloned().collect());
    order
        .into_iter()
        .filter_map(|n| groups.remove(&n).map(|s| (n, s)))
        .collect()
}

/// The order of `AnalysisRegistry.buildMapping`'s provider map: the configured names (in
/// settings order) followed by the built-in ones not configured.
fn provider_order(configured: &[String], built_in: &[&str]) -> Vec<String> {
    let mut names: Vec<String> = configured.to_vec();
    for b in built_in {
        if !configured.iter().any(|c| c == b) {
            names.push((*b).to_string());
        }
    }
    java_hashmap_order(names)
}

fn missing_type(kind: Kind, name: &str) -> AnalysisError {
    AnalysisError::illegal(format!(
        "{} [{name}] must specify either an analyzer type, or a tokenizer",
        kind.as_str()
    ))
}

fn unknown_type(kind: &str, ty: &str, name: &str) -> AnalysisError {
    AnalysisError::illegal(format!("Unknown {kind} type [{ty}] for [{name}]"))
}

/// Builds a component of `kind` from its type and settings.
fn build_typed(
    ctx: &Ctx,
    kind: Kind,
    ty: &str,
    name: &str,
    s: &Settings,
) -> Option<Result<Component>> {
    Some(match kind {
        Kind::CharFilter => {
            let p = registry::char_filter_provider(ty)?;
            (p.build)(ctx, name, s).map(|f| {
                Component::CharFilter(NamedCharFilter {
                    name: name.into(),
                    factory: f,
                })
            })
        }
        Kind::Tokenizer => {
            let p = registry::tokenizer_provider(ty)?;
            (p.build)(ctx, name, s).map(|f| {
                Component::Tokenizer(NamedTokenizer {
                    name: name.into(),
                    factory: f,
                })
            })
        }
        Kind::Filter => {
            let p = registry::filter_provider(ty)?;
            (p.build)(ctx, name, s).map(|f| {
                Component::Filter(NamedFilter {
                    name: name.into(),
                    factory: f,
                })
            })
        }
    })
}

fn requires_settings(kind: Kind, ty: &str) -> Option<bool> {
    match kind {
        Kind::CharFilter => registry::char_filter_provider(ty).map(|p| p.requires_settings),
        Kind::Tokenizer => registry::tokenizer_provider(ty).map(|p| p.requires_settings),
        Kind::Filter => registry::filter_provider(ty).map(|p| p.requires_settings),
    }
}

fn prebuilt(ctx: &Ctx, kind: Kind, name: &str) -> Option<Result<Component>> {
    Some(match kind {
        Kind::CharFilter => registry::prebuilt_char_filter(ctx, name)?.map(|f| {
            Component::CharFilter(NamedCharFilter {
                name: name.into(),
                factory: f,
            })
        }),
        Kind::Tokenizer => registry::prebuilt_named_tokenizer(ctx, name)?.map(Component::Tokenizer),
        Kind::Filter => registry::prebuilt_named_filter(ctx, name)?.map(Component::Filter),
    })
}

impl Component {
    fn tokenizer(self) -> NamedTokenizer {
        match self {
            Component::Tokenizer(t) => t,
            _ => unreachable!("component kind mismatch"),
        }
    }
    fn filter(self) -> NamedFilter {
        match self {
            Component::Filter(t) => t,
            _ => unreachable!("component kind mismatch"),
        }
    }
    fn char_filter(self) -> NamedCharFilter {
        match self {
            Component::CharFilter(t) => t,
            _ => unreachable!("component kind mismatch"),
        }
    }
}

impl Analysis {
    /// The analysis available without an index (`_analyze` with no index).
    pub fn global() -> &'static Analysis {
        static G: OnceLock<Analysis> = OnceLock::new();
        G.get_or_init(|| Analysis {
            inner: Arc::new(Inner {
                opts: IndexOptions::default(),
                global: true,
                settings: Settings::new(),
                char_filters: BTreeMap::new(),
                tokenizers: BTreeMap::new(),
                filters: BTreeMap::new(),
                analyzers: HashMap::new(),
                normalizers: HashMap::new(),
                whitespace_normalizers: HashMap::new(),
            }),
        })
    }

    /// The global analysis with custom hooks (scripts, synonym sets) for inline `_analyze`
    /// definitions that need them.
    pub fn global_with(opts: IndexOptions) -> Analysis {
        Analysis {
            inner: Arc::new(Inner {
                opts,
                global: true,
                settings: Settings::new(),
                char_filters: BTreeMap::new(),
                tokenizers: BTreeMap::new(),
                filters: BTreeMap::new(),
                analyzers: HashMap::new(),
                normalizers: HashMap::new(),
                whitespace_normalizers: HashMap::new(),
            }),
        }
    }

    /// Builds an index's analysis from its `index.analysis` settings (the object under
    /// `analysis`, nested or with dotted keys), failing with Elasticsearch's error for an invalid
    /// definition.
    pub fn from_settings(analysis: &serde_json::Value, opts: &IndexOptions) -> Result<Analysis> {
        let settings = Settings::from_json(analysis)?;
        let ctx = Ctx { opts };
        let mut inner = Inner {
            opts: opts.clone(),
            global: false,
            settings: settings.clone(),
            char_filters: BTreeMap::new(),
            tokenizers: BTreeMap::new(),
            filters: BTreeMap::new(),
            analyzers: HashMap::new(),
            normalizers: HashMap::new(),
            whitespace_normalizers: HashMap::new(),
        };
        // Elasticsearch builds (and so validates) components in a fixed order: char filters,
        // tokenizers, token filters, then the analyzer types, the normalizer types, then the
        // custom analyzers, then the custom normalizers; within each, in the iteration order
        // of the Java `HashMap`s involved. When several definitions are invalid, the error it
        // reports is the first one in that order, so the same order is followed here.
        for (kind, group) in [
            (Kind::CharFilter, "char_filter"),
            (Kind::Tokenizer, "tokenizer"),
            (Kind::Filter, "filter"),
        ] {
            for (name, s) in settings_groups(&settings, group) {
                let Some(ty) = s.get("type") else {
                    return Err(missing_type(kind, &name));
                };
                let c = build_typed(&ctx, kind, &ty, &name, &s)
                    .ok_or_else(|| unknown_type(kind.as_str(), &ty, &name))??;
                match c {
                    Component::CharFilter(c) => {
                        inner.char_filters.insert(name, c);
                    }
                    Component::Tokenizer(c) => {
                        inner.tokenizers.insert(name, c);
                    }
                    Component::Filter(c) => {
                        inner.filters.insert(name, c);
                    }
                }
            }
        }
        let analysis = Analysis {
            inner: Arc::new(inner),
        };
        // Analyzer and normalizer types (`AnalysisRegistry.buildMapping`): typed ones are
        // built now, custom ones only once every type is known.
        let analyzer_groups = settings_groups(&settings, "analyzer");
        let mut analyzers = HashMap::new();
        let mut custom = Vec::new();
        for (name, s) in &analyzer_groups {
            let ty = s.get("type");
            match ty.as_deref() {
                None if s.contains("tokenizer") => custom.push(name.clone()),
                None => {
                    return Err(AnalysisError::illegal(format!(
                        "analyzer [{name}] must specify either an analyzer type, or a tokenizer"
                    )));
                }
                Some("custom") => custom.push(name.clone()),
                Some(ty) => {
                    let p = analyzers::analyzer_provider(ty)
                        .ok_or_else(|| unknown_type("analyzer", ty, name))?;
                    analyzers.insert(name.clone(), p(&ctx, name, s)?);
                }
            }
        }
        let normalizer_groups = settings_groups(&settings, "normalizer");
        let mut normalizers = HashMap::new();
        let mut whitespace_normalizers = HashMap::new();
        let mut custom_normalizers = Vec::new();
        for (name, s) in &normalizer_groups {
            match s.get("type").as_deref() {
                None | Some("custom") => custom_normalizers.push(name.clone()),
                Some("lowercase") => {
                    normalizers.insert(name.clone(), lowercase_normalizer(name, false)?);
                    whitespace_normalizers.insert(name.clone(), lowercase_normalizer(name, true)?);
                }
                Some(ty) => return Err(unknown_type("normalizer", ty, name)),
            }
        }
        // `AnalysisRegistry.build`: every analyzer provider (configured ones and the built-in
        // ones), in the provider map's order.
        let configured: Vec<String> = analyzer_groups.iter().map(|(n, _)| n.clone()).collect();
        let providers = provider_order(&configured, analyzers::BUILT_IN_ANALYZERS);
        let groups: HashMap<&str, &Settings> = analyzer_groups
            .iter()
            .map(|(n, s)| (n.as_str(), s))
            .collect();
        for name in &providers {
            if custom.contains(name) {
                let a = analysis.build_custom(name, groups[name.as_str()])?;
                analyzers.insert(name.clone(), a);
            }
        }
        let configured: Vec<String> = normalizer_groups.iter().map(|(n, _)| n.clone()).collect();
        let ngroups: HashMap<&str, &Settings> = normalizer_groups
            .iter()
            .map(|(n, s)| (n.as_str(), s))
            .collect();
        for name in provider_order(&configured, &["lowercase"]) {
            if custom_normalizers.contains(&name) {
                let s = ngroups[name.as_str()];
                normalizers.insert(name.clone(), analysis.build_normalizer(&name, s, false)?);
                whitespace_normalizers
                    .insert(name.clone(), analysis.build_normalizer(&name, s, true)?);
            }
        }
        if !analyzers.contains_key("default") {
            analyzers.insert(
                "default".into(),
                analyzers::standard("default", Default::default(), 255)?,
            );
        }
        if let Some(d) = analyzers.get("default") {
            d.check_allowed_in_mode(crate::AnalysisMode::All)?;
        }
        if analyzers.contains_key("default_index") {
            let index = opts
                .index_name
                .as_deref()
                .map(|n| format!(" [{n}]"))
                .unwrap_or_default();
            return Err(AnalysisError::illegal(format!(
                "setting [index.analysis.analyzer.default_index] is not supported anymore, use [index.analysis.analyzer.default] instead for index{index}"
            )));
        }
        let mut with_default = providers.clone();
        if !with_default.iter().any(|n| n == "default") {
            with_default.push("default".into());
        }
        if let Some(name) = java_hashmap_order(with_default)
            .into_iter()
            .find(|k| k.starts_with('_'))
        {
            return Err(AnalysisError::illegal(format!(
                "analyzer name must not start with '_'. got \"{name}\""
            )));
        }
        let mut inner = Arc::try_unwrap(analysis.inner).unwrap_or_else(|a| clone_inner(&a));
        inner.analyzers = analyzers;
        inner.normalizers = normalizers;
        inner.whitespace_normalizers = whitespace_normalizers;
        Ok(Analysis {
            inner: Arc::new(inner),
        })
    }

    pub(crate) fn ctx(&self) -> Ctx<'_> {
        Ctx {
            opts: &self.inner.opts,
        }
    }

    /// An analyzer by name: configured ones first, then Elasticsearch's built-in analyzers.
    pub fn analyzer(&self, name: &str) -> Option<Analyzer> {
        if let Some(a) = self.inner.analyzers.get(name) {
            return Some(a.clone());
        }
        builtin_analyzer(name).and_then(|r| r.ok())
    }

    /// [`Analysis::analyzer`] for search-time use (an alias kept for symmetry with the
    /// index-time checks: search analyzers may contain search-time-only filters).
    pub fn search_analyzer(&self, name: &str) -> Option<Analyzer> {
        self.analyzer(name)
    }

    /// The `default` analyzer (configured, or `standard`).
    pub fn default_analyzer(&self) -> Analyzer {
        self.analyzer("default").unwrap_or_else(|| {
            analyzers::standard("default", Default::default(), 255)
                .expect("the standard analyzer always builds")
        })
    }

    /// `default_search` if configured.
    pub fn default_search_analyzer(&self) -> Option<Analyzer> {
        self.inner.analyzers.get("default_search").cloned()
    }

    /// `default_search_quoted` if configured.
    pub fn default_search_quote_analyzer(&self) -> Option<Analyzer> {
        self.inner.analyzers.get("default_search_quoted").cloned()
    }

    /// The names of the configured analyzers.
    pub fn analyzer_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.inner.analyzers.keys().cloned().collect();
        v.sort();
        v
    }

    /// A normalizer by name (configured, or the built-in `lowercase`).
    pub fn normalizer(&self, name: &str) -> Option<Normalizer> {
        if let Some(n) = self.inner.normalizers.get(name) {
            return Some(n.clone());
        }
        if name == "lowercase" && !self.inner.global {
            return lowercase_normalizer("lowercase", false).ok();
        }
        None
    }

    /// The normalizer variant that splits on whitespace first (for `split_queries_on_whitespace`).
    pub fn whitespace_normalizer(&self, name: &str) -> Option<Normalizer> {
        if let Some(n) = self.inner.whitespace_normalizers.get(name) {
            return Some(n.clone());
        }
        if name == "lowercase" && !self.inner.global {
            return lowercase_normalizer("lowercase", true).ok();
        }
        None
    }

    /// Resolves a component name in `scope`.
    pub(crate) fn resolve(&self, kind: Kind, name: &str, scope: Scope) -> Result<Component> {
        let ctx = self.ctx();
        let configured = match kind {
            Kind::CharFilter => self
                .inner
                .char_filters
                .get(name)
                .cloned()
                .map(Component::CharFilter),
            Kind::Tokenizer => self
                .inner
                .tokenizers
                .get(name)
                .cloned()
                .map(Component::Tokenizer),
            Kind::Filter => self.inner.filters.get(name).cloned().map(Component::Filter),
        };
        match scope {
            Scope::Index => {
                if let Some(c) = configured {
                    return Ok(c);
                }
                if let Some(c) = prebuilt(&ctx, kind, name) {
                    return c;
                }
                if requires_settings(kind, name) == Some(false) {
                    return build_typed(&ctx, kind, name, name, &Settings::new())
                        .expect("provider exists");
                }
                Err(AnalysisError::illegal(format!(
                    "failed to find {} under [{name}]",
                    kind.as_str()
                )))
            }
            Scope::AdHocIndex => {
                if let Some(c) = configured {
                    return Ok(c);
                }
                if let Some(c) = build_typed(&ctx, kind, name, name, &Settings::new()) {
                    return c;
                }
                if let Some(c) = prebuilt(&ctx, kind, name) {
                    return c;
                }
                Err(AnalysisError::illegal(format!(
                    "failed to find {} under [{name}]",
                    kind.as_str()
                )))
            }
            Scope::AdHocGlobal => {
                if let Some(c) = prebuilt(&ctx, kind, name) {
                    return c;
                }
                match requires_settings(kind, name) {
                    Some(false) => build_typed(&ctx, kind, name, name, &Settings::new())
                        .expect("provider exists"),
                    Some(true) => Err(AnalysisError::illegal(
                        "Analysis settings required - can't instantiate analysis factory",
                    )),
                    None => Err(AnalysisError::illegal(format!(
                        "failed to find global {} under [{name}]",
                        kind.as_str()
                    ))),
                }
            }
        }
    }

    /// An anonymous component definition (`{"type": …, …}` in `_analyze`).
    pub(crate) fn resolve_definition(&self, kind: Kind, def: &Settings) -> Result<Component> {
        let ctx = self.ctx();
        let Some(ty) = def.get("type") else {
            return Err(AnalysisError::illegal(format!(
                "Missing [type] setting for anonymous {}: {}",
                kind.as_str(),
                def.to_flat_json()
            )));
        };
        let name = format!("__anonymous__{ty}");
        // `AnalysisRegistry.get*Provider` falls back to the pre-configured component of that
        // name (which ignores the settings and keeps its own name).
        build_typed(&ctx, kind, &ty, &name, def)
            .or_else(|| prebuilt(&ctx, kind, &ty))
            .ok_or_else(|| {
                AnalysisError::illegal(format!(
                    "failed to find global {} under [{ty}]",
                    kind.as_str()
                ))
            })?
    }

    /// Finds a filter for chain-aware filters (`multiplexer`, `condition`): as an index analyzer
    /// sees it (None when unknown), or as `_analyze` resolves inline names (an error).
    pub(crate) fn filter_by_name(&self, name: &str, ad_hoc: bool) -> Result<Option<NamedFilter>> {
        if ad_hoc {
            let scope = if self.inner.global {
                Scope::AdHocGlobal
            } else {
                Scope::AdHocIndex
            };
            return self
                .resolve(Kind::Filter, name, scope)
                .map(|c| Some(c.filter()));
        }
        match self.resolve(Kind::Filter, name, Scope::Index) {
            Ok(c) => Ok(Some(c.filter())),
            Err(e) if e.reason.starts_with("failed to find") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `CustomAnalyzerProvider`.
    fn build_custom(&self, name: &str, s: &Settings) -> Result<Analyzer> {
        let pos_gap = s.get_int("position_increment_gap", POSITION_INCREMENT_GAP)?;
        let offset_gap = s.get_int("offset_gap", -1)?;
        let Some(tk) = s.get("tokenizer") else {
            return Err(AnalysisError::illegal(format!(
                "Custom Analyzer [{name}] must be configured with a tokenizer"
            )));
        };
        let tokenizer = self
            .resolve(Kind::Tokenizer, &tk, Scope::Index)
            .map(Component::tokenizer)
            .map_err(|e| {
                if e.reason.starts_with("failed to find") {
                    AnalysisError::illegal(format!(
                        "Custom Analyzer [{name}] failed to find tokenizer under name [{tk}]"
                    ))
                } else {
                    e
                }
            })?;
        let mut char_filters = Vec::new();
        for cf in s.list_or_empty("char_filter") {
            let c = self
                .resolve(Kind::CharFilter, &cf, Scope::Index)
                .map(Component::char_filter)
                .map_err(|e| {
                    if e.reason.starts_with("failed to find") {
                        AnalysisError::illegal(format!(
                            "Custom Analyzer [{name}] failed to find char_filter under name [{cf}]"
                        ))
                    } else {
                        e
                    }
                })?;
            char_filters.push(c);
        }
        let mut filters: Vec<NamedFilter> = Vec::new();
        for f in s.list_or_empty("filter") {
            let nf = self
                .resolve(Kind::Filter, &f, Scope::Index)
                .map(Component::filter)
                .map_err(|e| {
                    if e.reason.starts_with("failed to find") {
                        AnalysisError::illegal(format!(
                            "Custom Analyzer [{name}] failed to find filter under name [{f}]"
                        ))
                    } else {
                        e
                    }
                })?;
            let nf = self.chain_aware(nf, &char_filters, &tokenizer, &filters, false)?;
            filters.push(nf);
        }
        AnalyzerSpec {
            name: name.to_string(),
            char_filters,
            tokenizer,
            filters,
            custom: true,
            pos_gap,
            offset_gap: if offset_gap < 0 { 1 } else { offset_gap as u32 },
            norm_filters: None,
        }
        .build_with_explain_gap(pos_gap)
    }

    /// Applies `getChainAwareTokenFilterFactory`.
    pub(crate) fn chain_aware(
        &self,
        f: NamedFilter,
        char_filters: &[NamedCharFilter],
        tokenizer: &NamedTokenizer,
        previous: &[NamedFilter],
        ad_hoc: bool,
    ) -> Result<NamedFilter> {
        let parts = ChainParts {
            char_filters,
            tokenizer,
            filters: previous,
            referring: &[],
        };
        let resolve = |n: &str| self.filter_by_name(n, ad_hoc);
        Ok(match f.factory.chain_aware(&parts, &resolve)? {
            Some(factory) => NamedFilter {
                name: f.name,
                factory,
            },
            None => f,
        })
    }

    /// `CustomNormalizerProvider`.
    fn build_normalizer(&self, name: &str, s: &Settings, whitespace: bool) -> Result<Normalizer> {
        if s.contains("tokenizer") {
            return Err(AnalysisError::illegal(format!(
                "Custom normalizer [{name}] cannot configure a tokenizer"
            )));
        }
        let mut char_filters = Vec::new();
        for cf in s.list_or_empty("char_filter") {
            let c = self.resolve(Kind::CharFilter, &cf, Scope::Index).map(Component::char_filter).map_err(|e| {
                if e.reason.starts_with("failed to find") {
                    AnalysisError::illegal(format!("Custom normalizer [{name}] failed to find char_filter under name [{cf}]"))
                } else {
                    e
                }
            })?;
            if !c.factory.normalizing() {
                return Err(AnalysisError::illegal(format!(
                    "Custom normalizer [{name}] may not use char filter [{cf}]"
                )));
            }
            char_filters.push(c);
        }
        let mut filters = Vec::new();
        for f in s.list_or_empty("filter") {
            let nf = self
                .resolve(Kind::Filter, &f, Scope::Index)
                .map(Component::filter)
                .map_err(|e| {
                    if e.reason.starts_with("failed to find") {
                        AnalysisError::illegal(format!(
                            "Custom Analyzer [{name}] failed to find filter under name [{f}]"
                        ))
                    } else {
                        e
                    }
                })?;
            if !nf.factory.normalizing() {
                return Err(AnalysisError::illegal(format!(
                    "Custom normalizer [{name}] may not use filter [{f}]"
                )));
            }
            filters.push(nf);
        }
        let tokenizer = normalizer_tokenizer(whitespace);
        let analyzer = AnalyzerSpec {
            name: name.to_string(),
            char_filters,
            tokenizer,
            filters,
            custom: true,
            pos_gap: 0,
            offset_gap: 1,
            norm_filters: None,
        }
        .build_with_explain_gap(0)?;
        Ok(Normalizer { analyzer })
    }

    /// `AnalysisRegistry.buildCustomAnalyzer` for `_analyze` with inline components. Without a
    /// tokenizer the chain is a normalizer (keyword tokenizer, normalizing filters only).
    pub(crate) fn build_ad_hoc(
        &self,
        tokenizer: Option<&crate::analyze::NameOrDefinition>,
        char_filters: &[crate::analyze::NameOrDefinition],
        filters: &[crate::analyze::NameOrDefinition],
    ) -> Result<Analyzer> {
        let scope = if self.inner.global {
            Scope::AdHocGlobal
        } else {
            Scope::AdHocIndex
        };
        let normalizer = tokenizer.is_none();
        let get = |kind: Kind, nod: &crate::analyze::NameOrDefinition| -> Result<Component> {
            match nod {
                crate::analyze::NameOrDefinition::Name(n) => self.resolve(kind, n, scope),
                crate::analyze::NameOrDefinition::Definition(d) => {
                    let s = Settings::from_json(d).map_err(|e| {
                        AnalysisError::illegal(format!("Failed to parse [{d}]")).with_cause(e)
                    })?;
                    self.resolve_definition(kind, &s)
                }
            }
        };
        let tokenizer = match tokenizer {
            Some(t) => get(Kind::Tokenizer, t)?.tokenizer(),
            None => get(
                Kind::Tokenizer,
                &crate::analyze::NameOrDefinition::Name("keyword".into()),
            )?
            .tokenizer(),
        };
        let mut cfs = Vec::new();
        for c in char_filters {
            cfs.push(get(Kind::CharFilter, c)?.char_filter());
        }
        let mut fs: Vec<NamedFilter> = Vec::new();
        for f in filters {
            let nf = get(Kind::Filter, f)?.filter();
            if normalizer && !nf.factory.normalizing() {
                return Err(AnalysisError::illegal(format!(
                    "Custom normalizer may not use filter [{}]",
                    nf.name
                )));
            }
            let nf = self.chain_aware(nf, &cfs, &tokenizer, &fs, true)?;
            fs.push(nf);
        }
        AnalyzerSpec {
            name: "__custom__".into(),
            char_filters: cfs,
            tokenizer,
            filters: fs,
            custom: true,
            pos_gap: POSITION_INCREMENT_GAP,
            offset_gap: 1,
            norm_filters: None,
        }
        .build_with_explain_gap(0)
    }

    /// Builds an analyzer from an inline definition (`_analyze` with `tokenizer`/`filter`/
    /// `char_filter`, given as JSON: `{"tokenizer": …, "filter": [...], "char_filter": [...]}`).
    pub fn build_ad_hoc_json(&self, def: &serde_json::Value) -> Result<Analyzer> {
        let req = crate::analyze::AnalyzeRequest::components_from_json(def)?;
        self.build_ad_hoc(req.tokenizer.as_ref(), &req.char_filter, &req.filter)
    }

    /// The configured component names (for diagnostics).
    pub fn configured_components(&self) -> (Vec<String>, Vec<String>, Vec<String>) {
        (
            self.inner.char_filters.keys().cloned().collect(),
            self.inner.tokenizers.keys().cloned().collect(),
            self.inner.filters.keys().cloned().collect(),
        )
    }

    pub(crate) fn is_global(&self) -> bool {
        self.inner.global
    }

    /// The index options this analysis was built with.
    pub fn options(&self) -> &IndexOptions {
        &self.inner.opts
    }
}

fn clone_inner(a: &Inner) -> Inner {
    Inner {
        opts: a.opts.clone(),
        global: a.global,
        settings: a.settings.clone(),
        char_filters: a.char_filters.clone(),
        tokenizers: a.tokenizers.clone(),
        filters: a.filters.clone(),
        analyzers: a.analyzers.clone(),
        normalizers: a.normalizers.clone(),
        whitespace_normalizers: a.whitespace_normalizers.clone(),
    }
}

fn normalizer_tokenizer(whitespace: bool) -> NamedTokenizer {
    if whitespace {
        let p = Arc::new(tokenizer::Whitespace);
        NamedTokenizer {
            name: "whitespace".into(),
            factory: crate::component::tokenizer_fn(move || {
                Box::new(tokenizer::CharTokenizer::new(p.clone(), 255, false))
            }),
        }
    } else {
        NamedTokenizer {
            name: "keyword".into(),
            factory: crate::component::tokenizer_fn(|| {
                Box::new(tokenizer::KeywordTokenizer::new(256))
            }),
        }
    }
}

/// `LowercaseNormalizer` (the `lowercase` normalizer type).
fn lowercase_normalizer(name: &str, whitespace: bool) -> Result<Normalizer> {
    let analyzer = AnalyzerSpec {
        name: name.to_string(),
        char_filters: Vec::new(),
        tokenizer: normalizer_tokenizer(whitespace),
        filters: vec![NamedFilter {
            name: "lowercase".into(),
            factory: analyzers::lowercase(),
        }],
        custom: false,
        pos_gap: 0,
        offset_gap: 1,
        norm_filters: None,
    }
    .build_with_explain_gap(0)?;
    Ok(Normalizer { analyzer })
}

/// A built-in analyzer by name (pre-built, or a type with default settings).
pub(crate) fn builtin_analyzer(name: &str) -> Option<Result<Analyzer>> {
    if let Some(a) = analyzers::prebuilt(name) {
        return Some(a);
    }
    // Analyzer types without a pre-built instance are built on the fly, unnamed.
    let p = analyzers::analyzer_provider(name)?;
    let opts = IndexOptions::default();
    let mut s = Settings::new();
    s.put("type", name);
    Some(p(&Ctx { opts: &opts }, name, &s).map(Analyzer::anonymous))
}

// Silence unused warnings for trait objects referenced only in signatures.
#[allow(dead_code)]
fn _assert_traits(
    _: &dyn CharFilterFactory,
    _: &dyn TokenizerFactory,
    _: &dyn FilterFactory,
    _: SynonymRole,
) {
}
