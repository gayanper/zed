use std::{borrow::Cow, path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use collections::HashSet;
use futures::FutureExt as _;
use gpui::{App, AsyncApp};
#[doc(hidden)]
pub use language::LspAdapter;
use language::{
    LanguageConfig, LanguageName, LanguageQueries, LanguageRegistry, LanguageServerName,
    LoadedLanguage, LspAdapterDelegate, LspInstaller, QueryFile, QueryFileContents, Toolchain,
};
use lsp::{LanguageServerBinary, Uri};
use project::project_settings::ProjectSettings;
use rust_embed::RustEmbed;
use serde_json::Value;
use settings::{Settings as _, SettingsStore};

#[doc(hidden)]
pub use serde_json;

pub mod swift;

#[derive(RustEmbed)]
#[folder = "src/"]
#[exclude = "*.rs"]
#[cfg_attr(not(test), exclude = "lsp_dsl_test_*/*")]
struct LanguageDir;

/// A language declared with [`language!`].
#[derive(Clone, Copy)]
pub struct LanguageDecl {
    /// Folder under `src/` holding `config.toml` and the `*.scm` queries.
    /// The grammar is registered under this name, so `config.toml` must set `grammar = "<name>"`.
    pub name: &'static str,
    pub grammar: fn() -> tree_sitter::Language,
}

/// A language server declared with [`lsp_server!`].
#[derive(Clone, Copy)]
pub struct LspServerDecl {
    pub name: &'static str,
    pub languages: &'static [&'static str],
    pub initialization_options: fn() -> Option<Value>,
    pub workspace_configuration: fn() -> Option<Value>,
    pub custom_adapter: Option<fn() -> Arc<dyn LspAdapter>>,
}

/// Declares a language. Expands to `pub const LANGUAGE: LanguageDecl`.
#[macro_export]
macro_rules! language {
    (name: $name:literal, grammar: $grammar:expr $(,)?) => {
        pub const LANGUAGE: $crate::LanguageDecl = $crate::LanguageDecl {
            name: $name,
            grammar: || $grammar.into(),
        };
    };
}

/// Declares a language server. Expands to `pub const LSP_SERVER: LspServerDecl`.
///
/// Optional fields must appear in this order: `initialization_options`,
/// `workspace_configuration`, `adapter`. With `adapter:`, the other two are ignored
/// (the custom adapter provides them), and its `name()` must equal `name`.
#[macro_export]
macro_rules! lsp_server {
    (
        name: $name:literal,
        languages: [$($language:literal),* $(,)?]
        $(, initialization_options: $initialization_options:expr)?
        $(, workspace_configuration: $workspace_configuration:expr)?
        $(, adapter: $adapter:expr)?
        $(,)?
    ) => {
        pub const LSP_SERVER: $crate::LspServerDecl = $crate::LspServerDecl {
            name: $name,
            languages: &[$($language),*],
            initialization_options: || {
                None $(.or(Some({
                    #[allow(unused_imports)]
                    use $crate::serde_json::json;
                    $initialization_options
                })))?
            },
            workspace_configuration: || {
                None $(.or(Some({
                    #[allow(unused_imports)]
                    use $crate::serde_json::json;
                    $workspace_configuration
                })))?
            },
            custom_adapter: $crate::lsp_server!(@adapter $($adapter)?),
        };
    };
    (@adapter) => { None };
    (@adapter $adapter:expr) => {
        Some(|| ::std::sync::Arc::new($adapter) as ::std::sync::Arc<dyn $crate::LspAdapter>)
    };
}

/// Registers the given languages and servers: `register!(languages, cx, [a::LANGUAGE], [a::LSP_SERVER])`.
#[macro_export]
macro_rules! register {
    ($registry:expr, $cx:expr, [$($language:expr),* $(,)?], [$($server:expr),* $(,)?] $(,)?) => {
        $crate::register($registry, &[$($language),*], &[$($server),*], $cx)
    };
}

pub fn init(languages: Arc<LanguageRegistry>, cx: &mut App) {
    register!(languages, cx, [swift::LANGUAGE], [swift::LSP_SERVER]);
}

#[doc(hidden)]
pub fn register(
    registry: Arc<LanguageRegistry>,
    language_decls: &[LanguageDecl],
    server_decls: &[LspServerDecl],
    cx: &mut App,
) {
    for decl in language_decls {
        if let Err(error) = register_language(&registry, decl) {
            log::error!(
                "lsp_dsl: failed to register language {:?}: {error:#}",
                decl.name
            );
        }
    }

    if server_decls.is_empty() {
        return;
    }
    let servers = server_decls
        .iter()
        .filter_map(|decl| {
            let adapter = match decl.custom_adapter {
                Some(custom_adapter) => custom_adapter(),
                None => Arc::new(DslLspAdapter { decl: *decl }) as Arc<dyn LspAdapter>,
            };
            // Settings lookup and enable/disable key on `decl.name` while the registry keys
            // on `adapter.name()`, so a mismatch would leave servers running after disable.
            if adapter.name().0 != decl.name {
                log::error!(
                    "lsp_dsl: adapter name {:?} does not match declared server name {:?}; skipping",
                    adapter.name().0,
                    decl.name
                );
                return None;
            }
            Some((*decl, adapter))
        })
        .collect::<Vec<_>>();

    let mut enabled = HashSet::default();
    sync_servers(&registry, &servers, &mut enabled, cx);
    cx.observe_global::<SettingsStore>(move |cx| {
        sync_servers(&registry, &servers, &mut enabled, cx);
    })
    .detach();
}

fn register_language(registry: &LanguageRegistry, decl: &LanguageDecl) -> Result<()> {
    let config = load_config(decl.name)?;
    anyhow::ensure!(
        config.grammar.as_deref() == Some(decl.name),
        "config.toml must set grammar = {:?}",
        decl.name
    );
    if registry
        .grammar_names()
        .iter()
        .any(|grammar| grammar.as_ref() == decl.name)
    {
        log::warn!(
            "lsp_dsl: grammar {:?} replaces an already registered grammar with the same name",
            decl.name
        );
    }
    if language_collides(registry, &config.name) {
        log::warn!(
            "lsp_dsl: language {:?} replaces an already registered language with the same name",
            config.name.as_ref()
        );
    }
    registry.register_native_grammars([(decl.name, (decl.grammar)())]);
    let name = decl.name;
    registry.register_language(
        config.name.clone(),
        config.grammar.clone(),
        config.matcher.clone(),
        config.hidden,
        None,
        Arc::new(move || {
            let config = config.clone();
            async move {
                Ok(LoadedLanguage {
                    config,
                    queries: load_queries(name),
                    context_provider: None,
                    toolchain_provider: None,
                    manifest_name: None,
                })
            }
            .boxed()
        }),
    );
    Ok(())
}

fn language_collides(registry: &LanguageRegistry, name: &LanguageName) -> bool {
    registry.language_names().contains(name)
}

fn server_collides(registry: &LanguageRegistry, name: &LanguageServerName) -> bool {
    registry.adapter_for_name(name).is_some()
        || registry.available_lsp_adapter_names().contains(name)
}

fn load_config(name: &str) -> Result<LanguageConfig> {
    let file = LanguageDir::get(&format!("{name}/config.toml"))
        .with_context(|| format!("missing {name}/config.toml"))?;
    let config_toml = std::str::from_utf8(&file.data)?;
    LanguageConfig::from_toml(config_toml)
        .with_context(|| format!("failed to parse {name}/config.toml"))
}

fn load_queries(name: &str) -> LanguageQueries {
    LanguageQueries::from_files(LanguageDir::iter().filter_map(|path| {
        let file_name = path.strip_prefix(name)?.strip_prefix('/')?;
        let query_file = file_name.parse::<QueryFile>().ok()?;
        let contents = match LanguageDir::get(path.as_ref())?.data {
            Cow::Borrowed(bytes) => Cow::Borrowed(std::str::from_utf8(bytes).ok()?),
            Cow::Owned(bytes) => Cow::Owned(String::from_utf8(bytes).ok()?),
        };
        Some(QueryFileContents::new(query_file, contents))
    }))
}

/// A server is only registered while `lsp.<name>.binary.path` is set, so Zed never
/// tries to start (or download) it before the user configured a binary.
fn sync_servers(
    registry: &LanguageRegistry,
    servers: &[(LspServerDecl, Arc<dyn LspAdapter>)],
    enabled: &mut HashSet<&'static str>,
    cx: &App,
) {
    let settings = ProjectSettings::get_global(cx);
    for (decl, adapter) in servers {
        let server_name = LanguageServerName::new_static(decl.name);
        let has_path = settings
            .lsp
            .get(&server_name)
            .and_then(|lsp_settings| lsp_settings.binary.as_ref())
            .is_some_and(|binary| binary.path.is_some());

        if has_path && !enabled.contains(decl.name) {
            // Removing bumps the registry's reload count (re-assigning languages on open
            // buffers), so only remove when a same-named server actually exists.
            if server_collides(registry, &server_name) {
                log::warn!(
                    "lsp_dsl: language server {:?} replaces an already registered server with the same name",
                    decl.name
                );
                for language in decl.languages {
                    registry.remove_lsp_adapter(&LanguageName::new_static(language), &server_name);
                }
            }
            for language in decl.languages {
                registry.register_lsp_adapter(LanguageName::new_static(language), adapter.clone());
            }
            enabled.insert(decl.name);
            log::info!("lsp_dsl: enabled language server {:?}", decl.name);
        } else if !has_path && enabled.remove(decl.name) {
            for language in decl.languages {
                registry.remove_lsp_adapter(&LanguageName::new_static(language), &server_name);
            }
            log::info!("lsp_dsl: disabled language server {:?}", decl.name);
        }
    }
}

struct DslLspAdapter {
    decl: LspServerDecl,
}

impl LspInstaller for DslLspAdapter {
    type BinaryVersion = ();

    async fn fetch_latest_server_version(
        &self,
        _: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        _: &mut AsyncApp,
    ) -> Result<()> {
        anyhow::bail!(
            "language server {:?} has no binary; set lsp.{}.binary.path",
            self.decl.name,
            self.decl.name
        )
    }

    fn fetch_server_binary(
        &self,
        _: (),
        _: PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        let name = self.decl.name;
        async move { anyhow::bail!("language server {name:?} is never downloaded") }
    }

    async fn cached_server_binary(
        &self,
        _: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        None
    }
}

#[async_trait(?Send)]
impl LspAdapter for DslLspAdapter {
    fn name(&self) -> LanguageServerName {
        LanguageServerName::new_static(self.decl.name)
    }

    async fn initialization_options(
        self: Arc<Self>,
        _: &Arc<dyn LspAdapterDelegate>,
        _: &mut AsyncApp,
    ) -> Result<Option<Value>> {
        Ok((self.decl.initialization_options)())
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        _: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: Option<Uri>,
        _: &mut AsyncApp,
    ) -> Result<Value> {
        Ok((self.decl.workspace_configuration)().unwrap_or_else(|| serde_json::json!({})))
    }
}

#[cfg(test)]
mod lsp_dsl_tests;
