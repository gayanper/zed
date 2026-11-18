use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use fs::FakeFs;
use futures::{FutureExt as _, StreamExt as _, channel::mpsc::UnboundedReceiver};
use gpui::{AsyncApp, Entity, TestAppContext, UpdateGlobal as _};
use language::{
    FakeLspAdapter, LanguageMatcher, LanguageName, LanguageRegistry, LanguageServerName,
    LoadedLanguage, LspAdapter, LspAdapterDelegate, LspInstaller,
};
use lsp::{FakeLanguageServer, LanguageServerBinary};
use parking_lot::Mutex;
use project::Project;
use serde_json::{Value, json};
use settings::SettingsStore;
use util::path;

use crate::{LanguageDecl, LspServerDecl};

mod test_json {
    crate::language! {
        name: "lsp_dsl_test_json",
        grammar: tree_sitter_json::LANGUAGE,
    }
}

mod generated_server {
    crate::lsp_server! {
        name: "dsl-test-server",
        languages: ["DSL Test JSON"],
        initialization_options: json!({ "from": "declaration" }),
        workspace_configuration: json!({ "dslTest": { "enabled": true } }),
    }
}

mod custom_server {
    crate::lsp_server! {
        name: "dsl-custom-server",
        languages: ["DSL Test JSON"],
        adapter: super::CustomAdapter,
    }
}

struct CustomAdapter;

impl LspInstaller for CustomAdapter {
    type BinaryVersion = ();

    async fn fetch_latest_server_version(
        &self,
        _: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        _: &mut AsyncApp,
    ) -> Result<()> {
        anyhow::bail!("not downloadable")
    }

    fn fetch_server_binary(
        &self,
        _: (),
        _: PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        async { anyhow::bail!("not downloadable") }
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
impl LspAdapter for CustomAdapter {
    fn name(&self) -> LanguageServerName {
        LanguageServerName::new_static("dsl-custom-server")
    }

    async fn initialization_options(
        self: Arc<Self>,
        _: &Arc<dyn LspAdapterDelegate>,
        _: &mut AsyncApp,
    ) -> Result<Option<Value>> {
        Ok(Some(json!({ "from": "custom adapter" })))
    }
}

const LANGUAGE_NAME: &str = "DSL Test JSON";

fn init_test(cx: &mut TestAppContext) {
    zlog::init_test();
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });
}

fn set_user_settings(cx: &mut TestAppContext, settings: Value) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(&settings.to_string(), cx)
                .result()
                .expect("valid user settings");
        })
    });
}

fn register(
    registry: &Arc<LanguageRegistry>,
    languages: &[LanguageDecl],
    servers: &[LspServerDecl],
    cx: &mut TestAppContext,
) {
    cx.update(|cx| crate::register(registry.clone(), languages, servers, cx));
}

/// Registers a fake server for `name` that records the initialization options it receives.
fn fake_server(
    registry: &LanguageRegistry,
    name: &'static str,
) -> (
    UnboundedReceiver<FakeLanguageServer>,
    Arc<Mutex<Option<Value>>>,
) {
    let initialization_options = Arc::new(Mutex::new(None));
    let servers = registry.register_fake_lsp_server(
        LanguageServerName::new_static(name),
        lsp::ServerCapabilities::default(),
        Some(Box::new({
            let initialization_options = initialization_options.clone();
            move |server: &mut FakeLanguageServer| {
                let initialization_options = initialization_options.clone();
                server.set_request_handler::<lsp::request::Initialize, _, _>(move |params, _| {
                    *initialization_options.lock() = params.initialization_options;
                    async move { Ok(lsp::InitializeResult::default()) }
                });
            }
        })),
    );
    (servers, initialization_options)
}

async fn test_project(cx: &mut TestAppContext) -> Entity<Project> {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), json!({ "file.dsljson": "{}" }))
        .await;
    Project::test(fs, [path!("/root").as_ref()], cx).await
}

fn running_servers(project: &Entity<Project>, cx: &mut TestAppContext) -> usize {
    project.read_with(cx, |project, cx| {
        project.language_server_statuses(cx).count()
    })
}

#[gpui::test]
async fn test_language_is_registered(cx: &mut TestAppContext) {
    init_test(cx);
    let registry = Arc::new(LanguageRegistry::test(cx.executor()));
    register(&registry, &[test_json::LANGUAGE], &[], cx);

    let language = registry
        .language_for_name(LANGUAGE_NAME)
        .await
        .expect("language registered by name");
    assert_eq!(language.name(), LanguageName::new_static(LANGUAGE_NAME));
    assert!(
        language
            .grammar()
            .is_some_and(|grammar| grammar.highlights_config.is_some()),
        "highlights query should be loaded"
    );

    let by_path = registry
        .load_language_for_file_path(std::path::Path::new("some/file.dsljson"))
        .await
        .expect("language resolved by path suffix");
    assert_eq!(by_path.name(), language.name());
}

#[gpui::test]
async fn test_generated_server_lifecycle(cx: &mut TestAppContext) {
    init_test(cx);
    let project = test_project(cx).await;
    let registry = project.read_with(cx, |project, _| project.languages().clone());
    let (mut servers, initialization_options) = fake_server(&registry, "dsl-test-server");
    register(
        &registry,
        &[test_json::LANGUAGE],
        &[generated_server::LSP_SERVER],
        cx,
    );

    let (_buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/root/file.dsljson"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    assert!(
        servers.try_recv().is_err(),
        "no server should start without a binary path"
    );
    assert_eq!(running_servers(&project, cx), 0);
    assert!(
        registry
            .lsp_adapters(&LanguageName::new_static(LANGUAGE_NAME))
            .is_empty()
    );

    set_user_settings(
        cx,
        json!({
            "lsp": {
                "dsl-test-server": {
                    "binary": {
                        "path": path!("/opt/dsl/server"),
                        "arguments": ["--stdio", "--verbose"],
                    }
                }
            }
        }),
    );
    cx.run_until_parked();
    let server = servers
        .next()
        .await
        .expect("server started after setting the path");
    assert_eq!(server.binary.path, PathBuf::from(path!("/opt/dsl/server")));
    assert_eq!(
        server.binary.arguments,
        vec!["--stdio".into(), "--verbose".into()] as Vec<std::ffi::OsString>
    );
    assert_eq!(
        *initialization_options.lock(),
        Some(json!({ "from": "declaration" }))
    );

    let configuration = server
        .request::<lsp::request::WorkspaceConfiguration>(
            lsp::ConfigurationParams {
                items: vec![lsp::ConfigurationItem::default()],
            },
            lsp::DEFAULT_LSP_REQUEST_TIMEOUT,
        )
        .await
        .into_response()
        .unwrap();
    assert_eq!(
        configuration,
        vec![json!({ "dslTest": { "enabled": true } })]
    );
    assert_eq!(running_servers(&project, cx), 1);

    set_user_settings(cx, json!({}));
    cx.run_until_parked();
    assert_eq!(
        running_servers(&project, cx),
        0,
        "server stopped after removing the path"
    );

    set_user_settings(
        cx,
        json!({ "lsp": { "dsl-test-server": { "binary": { "path": "again" } } } }),
    );
    cx.run_until_parked();
    let server = servers
        .next()
        .await
        .expect("server restarted after re-enabling");
    assert_eq!(server.binary.path, PathBuf::from("again"));
    assert_eq!(running_servers(&project, cx), 1);
    assert_eq!(
        registry
            .lsp_adapters(&LanguageName::new_static(LANGUAGE_NAME))
            .len(),
        1
    );
}

#[gpui::test]
async fn test_custom_adapter(cx: &mut TestAppContext) {
    init_test(cx);
    set_user_settings(
        cx,
        json!({ "lsp": { "dsl-custom-server": { "binary": { "path": "custom-server" } } } }),
    );
    let project = test_project(cx).await;
    let registry = project.read_with(cx, |project, _| project.languages().clone());
    let (mut servers, initialization_options) = fake_server(&registry, "dsl-custom-server");
    register(
        &registry,
        &[test_json::LANGUAGE],
        &[custom_server::LSP_SERVER],
        cx,
    );

    let (_buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/root/file.dsljson"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    let server = servers.next().await.expect("custom server started");
    assert_eq!(server.binary.path, PathBuf::from("custom-server"));
    assert_eq!(
        *initialization_options.lock(),
        Some(json!({ "from": "custom adapter" }))
    );
}

#[gpui::test]
async fn test_collisions_are_won_by_lsp_dsl(cx: &mut TestAppContext) {
    init_test(cx);
    let registry = Arc::new(LanguageRegistry::test(cx.executor()));
    registry.register_language(
        LanguageName::new_static(LANGUAGE_NAME),
        None,
        Arc::new(LanguageMatcher {
            path_suffixes: vec!["other".into()],
            ..Default::default()
        }),
        false,
        None,
        Arc::new(|| {
            async {
                Ok(LoadedLanguage {
                    config: language::LanguageConfig {
                        name: LanguageName::new_static(LANGUAGE_NAME),
                        ..Default::default()
                    },
                    queries: Default::default(),
                    context_provider: None,
                    toolchain_provider: None,
                    manifest_name: None,
                })
            }
            .boxed()
        }),
    );
    registry.register_fake_lsp_adapter(
        LANGUAGE_NAME,
        FakeLspAdapter {
            name: "dsl-test-server",
            ..Default::default()
        },
    );
    // These checks gate the collision warnings logged during registration.
    assert!(crate::language_collides(
        &registry,
        &LanguageName::new_static(LANGUAGE_NAME)
    ));
    assert!(crate::server_collides(
        &registry,
        &LanguageServerName::new_static("dsl-test-server")
    ));
    set_user_settings(
        cx,
        json!({ "lsp": { "dsl-test-server": { "binary": { "path": "server" } } } }),
    );
    register(
        &registry,
        &[test_json::LANGUAGE],
        &[generated_server::LSP_SERVER],
        cx,
    );

    let language = registry.language_for_name(LANGUAGE_NAME).await.unwrap();
    assert!(
        language.grammar().is_some(),
        "lsp_dsl language replaced the earlier one"
    );
    assert!(
        registry
            .load_language_for_file_path(std::path::Path::new("file.dsljson"))
            .await
            .is_ok()
    );

    let adapters = registry.lsp_adapters(&LanguageName::new_static(LANGUAGE_NAME));
    assert_eq!(
        adapters.len(),
        1,
        "lsp_dsl server replaced the earlier adapter instead of being added next to it"
    );
}
