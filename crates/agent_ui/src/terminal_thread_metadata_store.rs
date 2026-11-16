use std::path::{Path, PathBuf};

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use collections::{HashMap, HashSet};
use db::{
    sqlez::{
        bindable::Column, domain::Domain, statement::Statement,
        thread_safe_connection::ThreadSafeConnection,
    },
    sqlez_macros::sql,
};
use futures::{FutureExt, future::Shared};
use gpui::{AppContext as _, Entity, Global, Task};
use remote::{RemoteConnectionOptions, same_remote_connection_identity};
use ui::{App, Context, SharedString};
use util::ResultExt as _;
use workspace::PathList;

use crate::{TerminalId, thread_metadata_store::WorktreePaths};

pub fn init(cx: &mut App) {
    TerminalThreadMetadataStore::init_global(cx);
}

struct GlobalTerminalThreadMetadataStore(Entity<TerminalThreadMetadataStore>);
impl Global for GlobalTerminalThreadMetadataStore {}

#[cfg(any(test, feature = "test-support"))]
pub struct TestTerminalMetadataDbName(pub String);
#[cfg(any(test, feature = "test-support"))]
impl Global for TestTerminalMetadataDbName {}

#[cfg(any(test, feature = "test-support"))]
impl TestTerminalMetadataDbName {
    pub fn global(cx: &App) -> String {
        cx.try_global::<Self>()
            .map(|global| global.0.clone())
            .unwrap_or_else(|| {
                let thread = std::thread::current();
                let test_name = thread.name().unwrap_or("unknown_test");
                format!("TERMINAL_THREAD_METADATA_DB_{}", test_name)
            })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TerminalThreadMetadata {
    pub terminal_id: TerminalId,
    pub title: SharedString,
    pub custom_title: Option<SharedString>,
    pub created_at: DateTime<Utc>,
    pub worktree_paths: WorktreePaths,
    pub remote_connection: Option<RemoteConnectionOptions>,
    pub working_directory: Option<PathBuf>,
    pub init_command: Option<String>,
    /// A lone terminal uses its own id, so it stays a valid group key after
    /// the terminal that founded the group closes.
    pub group_id: TerminalId,
    /// The open member with the lowest position holds the main slot. Gaps are allowed.
    pub position: i64,
    /// Whether this member is collapsed to its header in the group's overview.
    pub minimized_in_overview: bool,
}

/// Settings shared by every member of a terminal group, so they survive any
/// one member closing.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalGroup {
    pub group_id: TerminalId,
    pub overview_enabled: bool,
    pub custom_title: Option<SharedString>,
}

impl TerminalGroup {
    fn new(group_id: TerminalId) -> Self {
        Self {
            group_id,
            overview_enabled: false,
            custom_title: None,
        }
    }
}

impl TerminalThreadMetadata {
    pub fn folder_paths(&self) -> &PathList {
        self.worktree_paths.folder_path_list()
    }

    pub fn main_worktree_paths(&self) -> &PathList {
        self.worktree_paths.main_worktree_path_list()
    }

    pub fn display_title(&self) -> SharedString {
        compose_terminal_thread_title(
            self.title.as_ref(),
            self.custom_title.as_ref().map(|title| title.as_ref()),
        )
    }

    pub fn editable_title(&self) -> SharedString {
        self.custom_title.clone().unwrap_or_else(|| {
            SharedString::from(terminal_title_without_prefix(self.title.as_ref()).to_string())
        })
    }
}

pub(crate) fn compose_terminal_thread_title(
    terminal_title: &str,
    custom_title: Option<&str>,
) -> SharedString {
    let Some(custom_title) = custom_title.filter(|title| !title.trim().is_empty()) else {
        return SharedString::from(terminal_title.to_string());
    };

    if let Some(prefix) = terminal_title_prefix(terminal_title) {
        SharedString::from(format!("{prefix}{custom_title}"))
    } else {
        SharedString::from(custom_title.to_string())
    }
}

pub(crate) fn terminal_title_without_prefix(title: &str) -> &str {
    terminal_title_prefix(title)
        .map(|prefix| &title[prefix.len()..])
        .unwrap_or(title)
}

pub(crate) fn normalize_terminal_custom_title(
    terminal_title: &str,
    edited_title: SharedString,
) -> Option<SharedString> {
    if edited_title.trim().is_empty()
        || edited_title == terminal_title_without_prefix(terminal_title)
    {
        None
    } else {
        Some(edited_title)
    }
}

pub fn terminal_title_prefix(title: &str) -> Option<&str> {
    let mut prefix_byte_len = 0;
    let mut saw_prefix_character = false;
    let mut saw_whitespace_after_prefix = false;

    let mut chars = title.chars().peekable();
    while let Some(character) = chars.next() {
        if character.is_alphanumeric() {
            return None;
        }

        if character.is_whitespace() {
            if !saw_prefix_character {
                return None;
            }

            prefix_byte_len += character.len_utf8();
            saw_whitespace_after_prefix = true;

            while let Some(character) = chars.peek() {
                if !character.is_whitespace() {
                    break;
                }

                prefix_byte_len += character.len_utf8();
                chars.next();
            }

            break;
        }

        saw_prefix_character = true;
        prefix_byte_len += character.len_utf8();
    }

    if saw_whitespace_after_prefix {
        Some(&title[..prefix_byte_len])
    } else {
        None
    }
}

pub struct TerminalThreadMetadataStore {
    db: TerminalThreadMetadataDb,
    terminals: HashMap<TerminalId, TerminalThreadMetadata>,
    terminals_by_paths: HashMap<PathList, HashSet<TerminalId>>,
    terminals_by_main_paths: HashMap<PathList, HashSet<TerminalId>>,
    terminals_by_group: HashMap<TerminalId, HashSet<TerminalId>>,
    groups: HashMap<TerminalId, TerminalGroup>,
    reload_task: Option<Shared<Task<()>>>,
    pending_terminal_ops_tx: async_channel::Sender<DbOperation>,
    _db_operations_task: Task<()>,
}

#[derive(Debug, PartialEq)]
enum DbOperation {
    Upsert(TerminalThreadMetadata),
    Delete(TerminalId),
    UpsertGroup(TerminalGroup),
    DeleteGroup(TerminalId),
}

/// A lone terminal's group id equals its terminal id, so terminal and group
/// operations need distinct keys.
#[derive(Debug, PartialEq, Eq, Hash)]
enum DbOperationKey {
    Terminal(TerminalId),
    Group(TerminalId),
}

impl DbOperation {
    fn key(&self) -> DbOperationKey {
        match self {
            DbOperation::Upsert(metadata) => DbOperationKey::Terminal(metadata.terminal_id),
            DbOperation::Delete(terminal_id) => DbOperationKey::Terminal(*terminal_id),
            DbOperation::UpsertGroup(group) => DbOperationKey::Group(group.group_id),
            DbOperation::DeleteGroup(group_id) => DbOperationKey::Group(*group_id),
        }
    }
}

impl TerminalThreadMetadataStore {
    #[cfg(not(any(test, feature = "test-support")))]
    pub fn init_global(cx: &mut App) {
        if cx.has_global::<GlobalTerminalThreadMetadataStore>() {
            return;
        }

        let db = TerminalThreadMetadataDb::global(cx);
        let terminal_store = cx.new(|cx| Self::new(db, cx));
        cx.set_global(GlobalTerminalThreadMetadataStore(terminal_store));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn init_global(cx: &mut App) {
        let db_name = TestTerminalMetadataDbName::global(cx);
        let db = gpui::block_on(db::open_test_db::<TerminalThreadMetadataDb>(&db_name));
        let terminal_store = cx.new(|cx| Self::new(TerminalThreadMetadataDb(db), cx));
        cx.set_global(GlobalTerminalThreadMetadataStore(terminal_store));
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalTerminalThreadMetadataStore>()
            .map(|store| store.0.clone())
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalTerminalThreadMetadataStore>().0.clone()
    }

    pub fn entry(&self, terminal_id: TerminalId) -> Option<&TerminalThreadMetadata> {
        self.terminals.get(&terminal_id)
    }

    pub fn entries(&self) -> impl Iterator<Item = &TerminalThreadMetadata> + '_ {
        self.terminals.values()
    }

    pub fn reload_task(&self) -> Shared<Task<()>> {
        self.reload_task
            .clone()
            .unwrap_or_else(|| Task::ready(()).shared())
    }

    pub fn entries_for_path<'a>(
        &'a self,
        path_list: &PathList,
        remote_connection: Option<&'a RemoteConnectionOptions>,
    ) -> impl Iterator<Item = &'a TerminalThreadMetadata> + 'a {
        self.terminals_by_paths
            .get(path_list)
            .into_iter()
            .flatten()
            .filter_map(|id| self.terminals.get(id))
            .filter(move |terminal| {
                same_remote_connection_identity(
                    terminal.remote_connection.as_ref(),
                    remote_connection,
                )
            })
    }

    pub fn entries_for_main_worktree_path<'a>(
        &'a self,
        path_list: &PathList,
        remote_connection: Option<&'a RemoteConnectionOptions>,
    ) -> impl Iterator<Item = &'a TerminalThreadMetadata> + 'a {
        self.terminals_by_main_paths
            .get(path_list)
            .into_iter()
            .flatten()
            .filter_map(|id| self.terminals.get(id))
            .filter(move |terminal| {
                same_remote_connection_identity(
                    terminal.remote_connection.as_ref(),
                    remote_connection,
                )
            })
    }

    pub fn path_is_referenced_by_terminal(
        &self,
        except_terminal_ids: &[TerminalId],
        path: &Path,
        remote_connection: Option<&RemoteConnectionOptions>,
    ) -> bool {
        self.entries().any(|terminal| {
            !except_terminal_ids.contains(&terminal.terminal_id)
                && same_remote_connection_identity(
                    terminal.remote_connection.as_ref(),
                    remote_connection,
                )
                && terminal
                    .folder_paths()
                    .paths()
                    .iter()
                    .any(|folder_path| folder_path.as_path() == path)
        })
    }

    pub fn save(&mut self, metadata: TerminalThreadMetadata, cx: &mut Context<Self>) {
        self.save_internal(metadata);
        cx.notify();
    }

    pub fn rename_terminal(
        &mut self,
        terminal_id: TerminalId,
        title: SharedString,
        cx: &mut Context<Self>,
    ) {
        let Some(mut metadata) = self.entry(terminal_id).cloned() else {
            return;
        };
        let custom_title = normalize_terminal_custom_title(metadata.title.as_ref(), title);
        if metadata.custom_title == custom_title {
            return;
        }

        metadata.custom_title = custom_title;
        self.save_internal(metadata);
        cx.notify();
    }

    pub fn set_minimized_in_overview(
        &mut self,
        terminal_id: TerminalId,
        minimized_in_overview: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(mut metadata) = self.entry(terminal_id).cloned() else {
            return;
        };
        if metadata.minimized_in_overview == minimized_in_overview {
            return;
        }

        metadata.minimized_in_overview = minimized_in_overview;
        self.save_internal(metadata);
        cx.notify();
    }

    pub fn group(&self, group_id: TerminalId) -> Option<&TerminalGroup> {
        self.groups.get(&group_id)
    }

    /// Members in main-slot order: by position, with creation time and id
    /// breaking ties so the order is stable.
    pub fn group_members(&self, group_id: TerminalId) -> Vec<&TerminalThreadMetadata> {
        let mut members = self
            .terminals_by_group
            .get(&group_id)
            .into_iter()
            .flatten()
            .filter_map(|terminal_id| self.terminals.get(terminal_id))
            .collect::<Vec<_>>();
        members.sort_by_key(|member| (member.position, member.created_at, member.terminal_id));
        members
    }

    /// The oldest member, whose title names the group unless the group has a
    /// custom title.
    pub fn group_title_member(&self, group_id: TerminalId) -> Option<&TerminalThreadMetadata> {
        self.terminals_by_group
            .get(&group_id)
            .into_iter()
            .flatten()
            .filter_map(|terminal_id| self.terminals.get(terminal_id))
            .min_by_key(|member| (member.created_at, member.position, member.terminal_id))
    }

    pub fn group_display_title(&self, group_id: TerminalId) -> Option<SharedString> {
        let member = self.group_title_member(group_id)?;
        match self
            .group(group_id)
            .and_then(|group| group.custom_title.as_ref())
        {
            Some(custom_title) => Some(compose_terminal_thread_title(
                member.title.as_ref(),
                Some(custom_title.as_ref()),
            )),
            None => Some(member.display_title()),
        }
    }

    pub fn group_editable_title(&self, group_id: TerminalId) -> Option<SharedString> {
        match self
            .group(group_id)
            .and_then(|group| group.custom_title.clone())
        {
            Some(custom_title) => Some(custom_title),
            None => Some(self.group_title_member(group_id)?.editable_title()),
        }
    }

    pub fn set_group_overview(
        &mut self,
        group_id: TerminalId,
        overview_enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let mut group = self
            .groups
            .get(&group_id)
            .cloned()
            .unwrap_or_else(|| TerminalGroup::new(group_id));
        if self.groups.contains_key(&group_id) && group.overview_enabled == overview_enabled {
            return;
        }
        group.overview_enabled = overview_enabled;
        self.save_group_internal(group);
        cx.notify();
    }

    /// Renames the group's sidebar row without changing any member's title.
    pub fn rename_group(&mut self, group_id: TerminalId, title: SharedString, cx: &mut Context<Self>) {
        let Some(member_title) = self
            .group_title_member(group_id)
            .map(|member| member.title.clone())
        else {
            return;
        };
        let custom_title = normalize_terminal_custom_title(member_title.as_ref(), title);
        let mut group = self
            .groups
            .get(&group_id)
            .cloned()
            .unwrap_or_else(|| TerminalGroup::new(group_id));
        if self.groups.contains_key(&group_id) && group.custom_title == custom_title {
            return;
        }
        group.custom_title = custom_title;
        self.save_group_internal(group);
        cx.notify();
    }

    pub fn change_worktree_paths(
        &mut self,
        current_folder_paths: &PathList,
        remote_connection: Option<&RemoteConnectionOptions>,
        mutate: impl Fn(&mut WorktreePaths),
        cx: &mut Context<Self>,
    ) {
        let terminal_ids: Vec<_> = self
            .terminals_by_paths
            .get(current_folder_paths)
            .into_iter()
            .flatten()
            .filter(|id| {
                self.terminals.get(id).is_some_and(|terminal| {
                    same_remote_connection_identity(
                        terminal.remote_connection.as_ref(),
                        remote_connection,
                    )
                })
            })
            .copied()
            .collect();

        if terminal_ids.is_empty() {
            return;
        }

        for terminal_id in terminal_ids {
            if let Some(mut terminal) = self.terminals.get(&terminal_id).cloned() {
                mutate(&mut terminal.worktree_paths);
                self.save_internal(terminal);
            }
        }

        cx.notify();
    }

    fn save_internal(&mut self, metadata: TerminalThreadMetadata) {
        if let Some(existing) = self.terminals.get(&metadata.terminal_id) {
            if existing.folder_paths() != metadata.folder_paths()
                && let Some(ids) = self.terminals_by_paths.get_mut(existing.folder_paths())
            {
                ids.remove(&metadata.terminal_id);
            }

            if existing.main_worktree_paths() != metadata.main_worktree_paths()
                && let Some(ids) = self
                    .terminals_by_main_paths
                    .get_mut(existing.main_worktree_paths())
            {
                ids.remove(&metadata.terminal_id);
            }

            if existing.group_id != metadata.group_id {
                let old_group_id = existing.group_id;
                self.remove_from_group(old_group_id, metadata.terminal_id);
            }
        }

        let group_id = metadata.group_id;
        self.cache_terminal_metadata(metadata.clone());
        self.pending_terminal_ops_tx
            .try_send(DbOperation::Upsert(metadata))
            .log_err();
        let member_count = self
            .terminals_by_group
            .get(&group_id)
            .map_or(0, |ids| ids.len());
        if member_count > 1 && !self.groups.contains_key(&group_id) {
            self.save_group_internal(TerminalGroup::new(group_id));
        }
    }

    fn save_group_internal(&mut self, group: TerminalGroup) {
        self.groups.insert(group.group_id, group.clone());
        self.pending_terminal_ops_tx
            .try_send(DbOperation::UpsertGroup(group))
            .log_err();
    }

    fn remove_from_group(&mut self, group_id: TerminalId, terminal_id: TerminalId) {
        let Some(ids) = self.terminals_by_group.get_mut(&group_id) else {
            return;
        };
        ids.remove(&terminal_id);
        if ids.is_empty() {
            self.terminals_by_group.remove(&group_id);
            if self.groups.remove(&group_id).is_some() {
                self.pending_terminal_ops_tx
                    .try_send(DbOperation::DeleteGroup(group_id))
                    .log_err();
            }
        }
    }

    fn cache_terminal_metadata(&mut self, metadata: TerminalThreadMetadata) {
        self.terminals
            .insert(metadata.terminal_id, metadata.clone());

        self.terminals_by_paths
            .entry(metadata.folder_paths().clone())
            .or_default()
            .insert(metadata.terminal_id);

        self.terminals_by_group
            .entry(metadata.group_id)
            .or_default()
            .insert(metadata.terminal_id);

        if !metadata.main_worktree_paths().is_empty() {
            self.terminals_by_main_paths
                .entry(metadata.main_worktree_paths().clone())
                .or_default()
                .insert(metadata.terminal_id);
        }
    }

    pub fn delete(&mut self, terminal_id: TerminalId, cx: &mut Context<Self>) {
        if let Some(terminal) = self.terminals.remove(&terminal_id) {
            if let Some(ids) = self.terminals_by_paths.get_mut(terminal.folder_paths()) {
                ids.remove(&terminal_id);
            }
            if !terminal.main_worktree_paths().is_empty()
                && let Some(ids) = self
                    .terminals_by_main_paths
                    .get_mut(terminal.main_worktree_paths())
            {
                ids.remove(&terminal_id);
            }
            self.pending_terminal_ops_tx
                .try_send(DbOperation::Delete(terminal_id))
                .log_err();
            self.remove_from_group(terminal.group_id, terminal_id);
        } else {
            self.pending_terminal_ops_tx
                .try_send(DbOperation::Delete(terminal_id))
                .log_err();
        }
        cx.notify();
    }

    fn new(db: TerminalThreadMetadataDb, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::unbounded();
        let _db_operations_task = cx.background_spawn({
            let db = db.clone();
            async move {
                while let Ok(first_update) = rx.recv().await {
                    let mut updates = vec![first_update];
                    while let Ok(update) = rx.try_recv() {
                        updates.push(update);
                    }
                    let updates = Self::dedup_db_operations(updates);
                    for operation in updates {
                        match operation {
                            DbOperation::Upsert(metadata) => {
                                db.save(metadata).await.log_err();
                            }
                            DbOperation::Delete(terminal_id) => {
                                db.delete(terminal_id).await.log_err();
                            }
                            DbOperation::UpsertGroup(group) => {
                                db.save_group(group).await.log_err();
                            }
                            DbOperation::DeleteGroup(group_id) => {
                                db.delete_group(group_id).await.log_err();
                            }
                        }
                    }
                }
            }
        });

        let mut this = Self {
            db,
            terminals: HashMap::default(),
            terminals_by_paths: HashMap::default(),
            terminals_by_main_paths: HashMap::default(),
            terminals_by_group: HashMap::default(),
            groups: HashMap::default(),
            reload_task: None,
            pending_terminal_ops_tx: tx,
            _db_operations_task,
        };
        this.reload(cx);
        this
    }

    fn dedup_db_operations(operations: Vec<DbOperation>) -> Vec<DbOperation> {
        let mut ops = HashMap::default();
        for operation in operations.into_iter().rev() {
            if ops.contains_key(&operation.key()) {
                continue;
            }
            ops.insert(operation.key(), operation);
        }
        ops.into_values().collect()
    }

    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        let db = self.db.clone();
        self.reload_task = Some(
            cx.spawn(async move |this, cx| {
                let (rows, groups) = cx
                    .background_spawn(async move {
                        let rows = db
                            .list()
                            .context("Failed to fetch terminal thread metadata")?;
                        let groups = db
                            .list_groups()
                            .context("Failed to fetch terminal groups")?;
                        anyhow::Ok((rows, groups))
                    })
                    .await
                    .log_err()
                    .unwrap_or_default();

                this.update(cx, |this, cx| {
                    this.terminals.clear();
                    this.terminals_by_paths.clear();
                    this.terminals_by_main_paths.clear();
                    this.terminals_by_group.clear();
                    this.groups.clear();

                    for row in rows {
                        this.cache_terminal_metadata(row);
                    }
                    for group in groups {
                        this.groups.insert(group.group_id, group);
                    }

                    cx.notify();
                })
                .ok();
            })
            .shared(),
        );
    }
}

struct TerminalThreadMetadataDb(ThreadSafeConnection);

impl Domain for TerminalThreadMetadataDb {
    const NAME: &str = stringify!(TerminalThreadMetadataDb);

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE IF NOT EXISTS sidebar_terminal_threads(
                terminal_id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                custom_title TEXT,
                created_at TEXT NOT NULL,
                working_directory TEXT,
                folder_paths TEXT,
                folder_paths_order TEXT,
                main_worktree_paths TEXT,
                main_worktree_paths_order TEXT,
                remote_connection TEXT
            ) STRICT;
        ),
        sql!(
            ALTER TABLE sidebar_terminal_threads ADD COLUMN init_command TEXT;
        ),
        sql!(
            ALTER TABLE sidebar_terminal_threads ADD COLUMN parent_terminal_id TEXT;
        ),
        sql!(
            ALTER TABLE sidebar_terminal_threads ADD COLUMN overview_enabled INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE sidebar_terminal_threads ADD COLUMN shown_in_overview INTEGER NOT NULL DEFAULT 0;
        ),
        sql!(
            ALTER TABLE sidebar_terminal_threads ADD COLUMN minimized_in_overview INTEGER NOT NULL DEFAULT 0;
        ),
        // A sub terminal joins its parent's group. One whose parent is gone
        // becomes a group of one, as it already showed as its own row.
        sql!(
            ALTER TABLE sidebar_terminal_threads ADD COLUMN group_id TEXT;
            UPDATE sidebar_terminal_threads SET group_id = CASE
                WHEN EXISTS (
                    SELECT parent.terminal_id FROM sidebar_terminal_threads AS parent
                    WHERE parent.terminal_id = sidebar_terminal_threads.parent_terminal_id
                ) THEN parent_terminal_id
                ELSE terminal_id
            END;
        ),
        // The old parent keeps position 0 and its sub terminals follow in
        // creation order, matching the order the overview showed them in.
        sql!(
            ALTER TABLE sidebar_terminal_threads ADD COLUMN position INTEGER NOT NULL DEFAULT 0;
            UPDATE sidebar_terminal_threads SET position = (
                SELECT COUNT(*) FROM sidebar_terminal_threads AS sibling
                WHERE sibling.group_id = sidebar_terminal_threads.group_id
                    AND NOT (sibling.terminal_id = sibling.group_id)
                    AND NOT (
                        sibling.created_at > sidebar_terminal_threads.created_at
                        OR (
                            sibling.created_at = sidebar_terminal_threads.created_at
                            AND sibling.terminal_id > sidebar_terminal_threads.terminal_id
                        )
                    )
            )
            WHERE NOT (group_id = terminal_id);
        ),
        sql!(
            CREATE TABLE IF NOT EXISTS terminal_groups(
                group_id TEXT PRIMARY KEY,
                overview_enabled INTEGER NOT NULL DEFAULT 0,
                custom_title TEXT
            ) STRICT;
            INSERT INTO terminal_groups(group_id, overview_enabled, custom_title)
            SELECT parent.terminal_id, parent.overview_enabled, parent.custom_title
            FROM sidebar_terminal_threads AS parent
            WHERE EXISTS (
                SELECT member.terminal_id FROM sidebar_terminal_threads AS member
                WHERE member.group_id = parent.terminal_id
                    AND NOT (member.terminal_id = parent.terminal_id)
            );
        ),
        sql!(
            ALTER TABLE sidebar_terminal_threads DROP COLUMN parent_terminal_id;
            ALTER TABLE sidebar_terminal_threads DROP COLUMN overview_enabled;
        ),
    ];
}

db::static_connection!(TerminalThreadMetadataDb, []);

const LIST_TERMINALS_QUERY: &str = "SELECT terminal_id, title, custom_title, created_at, \
    working_directory, folder_paths, folder_paths_order, main_worktree_paths, \
    main_worktree_paths_order, remote_connection, init_command, group_id, position, \
    minimized_in_overview \
    FROM sidebar_terminal_threads \
    ORDER BY created_at DESC";

const LIST_GROUPS_QUERY: &str =
    "SELECT group_id, overview_enabled, custom_title FROM terminal_groups";

impl TerminalThreadMetadataDb {
    pub fn list(&self) -> anyhow::Result<Vec<TerminalThreadMetadata>> {
        self.select::<TerminalThreadMetadata>(LIST_TERMINALS_QUERY)?()
    }

    pub fn list_groups(&self) -> anyhow::Result<Vec<TerminalGroup>> {
        self.select::<TerminalGroup>(LIST_GROUPS_QUERY)?()
    }

    pub async fn save_group(&self, group: TerminalGroup) -> anyhow::Result<()> {
        let group_id = group.group_id.to_key_string();
        let overview_enabled = group.overview_enabled;
        let custom_title = group.custom_title.as_ref().map(ToString::to_string);
        self.write(move |conn| {
            let sql = "INSERT INTO terminal_groups(group_id, overview_enabled, custom_title) \
                       VALUES (?1, ?2, ?3) \
                       ON CONFLICT(group_id) DO UPDATE SET \
                           overview_enabled = excluded.overview_enabled, \
                           custom_title = excluded.custom_title";
            let mut stmt = Statement::prepare(conn, sql)?;
            let mut i = stmt.bind(&group_id, 1)?;
            i = stmt.bind(&overview_enabled, i)?;
            stmt.bind(&custom_title, i)?;
            stmt.exec()
        })
        .await
    }

    pub async fn delete_group(&self, group_id: TerminalId) -> anyhow::Result<()> {
        let group_id = group_id.to_key_string();
        self.write(move |conn| {
            let mut stmt =
                Statement::prepare(conn, "DELETE FROM terminal_groups WHERE group_id = ?")?;
            stmt.bind(&group_id, 1)?;
            stmt.exec()
        })
        .await
    }

    pub async fn save(&self, row: TerminalThreadMetadata) -> anyhow::Result<()> {
        let terminal_id = row.terminal_id.to_key_string();
        let title = row.title.to_string();
        let custom_title = row.custom_title.as_ref().map(ToString::to_string);
        let created_at = row.created_at.to_rfc3339();
        let working_directory = row
            .working_directory
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let serialized = row.folder_paths().serialize();
        let (folder_paths, folder_paths_order) = if row.folder_paths().is_empty() {
            (None, None)
        } else {
            (Some(serialized.paths), Some(serialized.order))
        };
        let main_serialized = row.main_worktree_paths().serialize();
        let (main_worktree_paths, main_worktree_paths_order) =
            if row.main_worktree_paths().is_empty() {
                (None, None)
            } else {
                (Some(main_serialized.paths), Some(main_serialized.order))
            };
        let remote_connection = row
            .remote_connection
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("serialize terminal thread remote connection")?;
        let init_command = row.init_command;
        let group_id = row.group_id.to_key_string();
        let position = row.position;
        let minimized_in_overview = row.minimized_in_overview;

        self.write(move |conn| {
            let sql = "INSERT INTO sidebar_terminal_threads(terminal_id, title, custom_title, created_at, working_directory, folder_paths, folder_paths_order, main_worktree_paths, main_worktree_paths_order, remote_connection, init_command, group_id, position, minimized_in_overview) \
                       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
                       ON CONFLICT(terminal_id) DO UPDATE SET \
                           title = excluded.title, \
                           custom_title = excluded.custom_title, \
                           created_at = excluded.created_at, \
                           working_directory = excluded.working_directory, \
                           folder_paths = excluded.folder_paths, \
                           folder_paths_order = excluded.folder_paths_order, \
                           main_worktree_paths = excluded.main_worktree_paths, \
                           main_worktree_paths_order = excluded.main_worktree_paths_order, \
                            remote_connection = excluded.remote_connection, \
                            init_command = excluded.init_command, \
                            group_id = excluded.group_id, \
                            position = excluded.position, \
                            minimized_in_overview = excluded.minimized_in_overview";
            let mut stmt = Statement::prepare(conn, sql)?;
            let mut i = stmt.bind(&terminal_id, 1)?;
            i = stmt.bind(&title, i)?;
            i = stmt.bind(&custom_title, i)?;
            i = stmt.bind(&created_at, i)?;
            i = stmt.bind(&working_directory, i)?;
            i = stmt.bind(&folder_paths, i)?;
            i = stmt.bind(&folder_paths_order, i)?;
            i = stmt.bind(&main_worktree_paths, i)?;
            i = stmt.bind(&main_worktree_paths_order, i)?;
            i = stmt.bind(&remote_connection, i)?;
            i = stmt.bind(&init_command, i)?;
            i = stmt.bind(&group_id, i)?;
            i = stmt.bind(&position, i)?;
            stmt.bind(&minimized_in_overview, i)?;
            stmt.exec()
        })
        .await
    }

    pub async fn delete(&self, terminal_id: TerminalId) -> anyhow::Result<()> {
        let terminal_id = terminal_id.to_key_string();
        self.write(move |conn| {
            let mut stmt = Statement::prepare(
                conn,
                "DELETE FROM sidebar_terminal_threads WHERE terminal_id = ?",
            )?;
            stmt.bind(&terminal_id, 1)?;
            stmt.exec()
        })
        .await
    }
}

impl Column for TerminalThreadMetadata {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (terminal_id, next): (String, i32) = Column::column(statement, start_index)?;
        let (title, next): (String, i32) = Column::column(statement, next)?;
        let (custom_title, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (created_at, next): (String, i32) = Column::column(statement, next)?;
        let (working_directory, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (folder_paths_str, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (folder_paths_order_str, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (main_worktree_paths_str, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (main_worktree_paths_order_str, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (remote_connection_json, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (init_command, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (group_id, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (position, next): (i64, i32) = Column::column(statement, next)?;
        let (minimized_in_overview, next): (bool, i32) = Column::column(statement, next)?;

        let folder_paths = folder_paths_str
            .map(|paths| {
                PathList::deserialize(&util::path_list::SerializedPathList {
                    paths,
                    order: folder_paths_order_str.unwrap_or_default(),
                })
            })
            .unwrap_or_default();

        let main_worktree_paths = main_worktree_paths_str
            .map(|paths| {
                PathList::deserialize(&util::path_list::SerializedPathList {
                    paths,
                    order: main_worktree_paths_order_str.unwrap_or_default(),
                })
            })
            .unwrap_or_default();

        let remote_connection = remote_connection_json
            .as_deref()
            .map(serde_json::from_str::<RemoteConnectionOptions>)
            .transpose()
            .context("deserialize terminal thread remote connection")?;

        let worktree_paths = WorktreePaths::from_path_lists(main_worktree_paths, folder_paths)
            .unwrap_or_else(|_| WorktreePaths::default());

        let terminal_id = TerminalId::from_key_string(&terminal_id)?;
        let group_id = match group_id.as_deref() {
            Some(group_id) => TerminalId::from_key_string(group_id)?,
            None => terminal_id,
        };

        Ok((
            TerminalThreadMetadata {
                terminal_id,
                title: SharedString::from(title),
                custom_title: custom_title
                    .filter(|title| !title.trim().is_empty())
                    .map(SharedString::from),
                created_at: DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc),
                worktree_paths,
                remote_connection,
                working_directory: working_directory.map(PathBuf::from),
                init_command,
                group_id,
                position,
                minimized_in_overview,
            },
            next,
        ))
    }
}

impl Column for TerminalGroup {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (group_id, next): (String, i32) = Column::column(statement, start_index)?;
        let (overview_enabled, next): (bool, i32) = Column::column(statement, next)?;
        let (custom_title, next): (Option<String>, i32) = Column::column(statement, next)?;
        Ok((
            TerminalGroup {
                group_id: TerminalId::from_key_string(&group_id)?,
                overview_enabled,
                custom_title: custom_title
                    .filter(|title| !title.trim().is_empty())
                    .map(SharedString::from),
            },
            next,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::path::Path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            TerminalThreadMetadataStore::init_global(cx);
        });
        cx.run_until_parked();
    }

    fn metadata(title: &str, worktree_paths: WorktreePaths) -> TerminalThreadMetadata {
        let now = Utc::now();
        let terminal_id = TerminalId::new();
        TerminalThreadMetadata {
            terminal_id,
            title: SharedString::from(title.to_string()),
            custom_title: None,
            created_at: now,
            worktree_paths,
            remote_connection: None,
            working_directory: None,
            init_command: None,
            group_id: terminal_id,
            position: 0,
            minimized_in_overview: false,
        }
    }

    fn member_of(
        title: &str,
        group: &TerminalThreadMetadata,
        position: i64,
    ) -> TerminalThreadMetadata {
        let mut member = metadata(title, group.worktree_paths.clone());
        member.group_id = group.group_id;
        member.position = position;
        member
    }

    fn reload_store(cx: &mut TestAppContext) {
        cx.run_until_parked();
        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| store.reload(cx));
        });
        cx.run_until_parked();
    }

    #[test]
    fn test_terminal_title_prefix_preserves_non_alphanumeric_prefixes() {
        assert_eq!(terminal_title_prefix("✳ Thinking"), Some("✳ "));
        assert_eq!(terminal_title_prefix(">>>   Thinking"), Some(">>>   "));
        assert_eq!(terminal_title_prefix("⠋ Running"), Some("⠋ "));
        assert_eq!(terminal_title_prefix("* Claude"), Some("* "));
        assert_eq!(terminal_title_prefix("✳Thinking"), None);
        assert_eq!(terminal_title_prefix("Thinking"), None);
        assert_eq!(terminal_title_prefix(" Thinking"), None);
        assert_eq!(terminal_title_prefix("✳"), None);
        assert_eq!(terminal_title_prefix("v1 Running"), None);
    }

    #[test]
    fn test_terminal_thread_display_title_combines_raw_and_custom_titles() {
        let mut metadata = metadata(
            "⠋ Thinking",
            WorktreePaths::from_folder_paths(&PathList::default()),
        );
        metadata.custom_title = Some("Fix bug".into());
        assert_eq!(metadata.display_title().as_ref(), "⠋ Fix bug");

        metadata.title = "Thinking".into();
        assert_eq!(metadata.display_title().as_ref(), "Fix bug");
    }

    #[gpui::test]
    async fn test_rename_terminal_updates_stored_custom_title(cx: &mut TestAppContext) {
        init_test(cx);

        let metadata = metadata(
            "⠋ Dev Server",
            WorktreePaths::from_folder_paths(&PathList::default()),
        );
        let terminal_id = metadata.terminal_id;

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(metadata, cx);
                store.rename_terminal(terminal_id, "Renamed Terminal".into(), cx);
            });
        });

        cx.update(|cx| {
            let store = TerminalThreadMetadataStore::global(cx);
            let metadata = store
                .read(cx)
                .entry(terminal_id)
                .expect("renamed terminal metadata should exist");
            assert_eq!(metadata.custom_title.as_deref(), Some("Renamed Terminal"));
            assert_eq!(metadata.display_title().as_ref(), "⠋ Renamed Terminal");
        });

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.rename_terminal(terminal_id, "Dev Server".into(), cx);
            });
            let store = TerminalThreadMetadataStore::global(cx);
            let metadata = store
                .read(cx)
                .entry(terminal_id)
                .expect("renamed terminal metadata should exist");
            assert_eq!(metadata.custom_title, None);
            assert_eq!(metadata.display_title().as_ref(), "⠋ Dev Server");
        });
    }

    #[gpui::test]
    async fn test_terminal_init_command_persists(cx: &mut TestAppContext) {
        init_test(cx);
        let mut metadata = metadata(
            "Dev Server",
            WorktreePaths::from_folder_paths(&PathList::default()),
        );
        metadata.init_command = Some("claude --resume".to_string());
        let terminal_id = metadata.terminal_id;

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(metadata, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| store.reload(cx));
        });
        cx.run_until_parked();

        cx.update(|cx| {
            let store = TerminalThreadMetadataStore::global(cx);
            assert_eq!(
                store
                    .read(cx)
                    .entry(terminal_id)
                    .expect("terminal metadata should exist")
                    .init_command
                    .as_deref(),
                Some("claude --resume")
            );
        });
    }

    #[gpui::test]
    async fn test_terminal_group_persists(cx: &mut TestAppContext) {
        init_test(cx);
        let founder = metadata(
            "Founder",
            WorktreePaths::from_folder_paths(&PathList::default()),
        );
        let member = member_of("Member", &founder, 1);
        let group_id = founder.group_id;
        let founder_id = founder.terminal_id;
        let member_id = member.terminal_id;

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(founder, cx);
                assert!(
                    store.group(group_id).is_none(),
                    "a lone terminal has no group row"
                );
                store.save(member, cx);
                store.set_group_overview(group_id, true, cx);
                store.set_minimized_in_overview(member_id, true, cx);
            });
        });
        reload_store(cx);

        cx.update(|cx| {
            let store = TerminalThreadMetadataStore::global(cx);
            let store = store.read(cx);
            assert_eq!(
                store
                    .group_members(group_id)
                    .iter()
                    .map(|member| (member.terminal_id, member.position))
                    .collect::<Vec<_>>(),
                vec![(founder_id, 0), (member_id, 1)]
            );
            assert!(store.group(group_id).expect("group row").overview_enabled);
            assert!(store.entry(member_id).unwrap().minimized_in_overview);
            assert!(!store.entry(founder_id).unwrap().minimized_in_overview);
        });
    }

    #[gpui::test]
    async fn test_terminal_group_outlives_founder(cx: &mut TestAppContext) {
        init_test(cx);
        let founder = metadata(
            "Founder",
            WorktreePaths::from_folder_paths(&PathList::default()),
        );
        let member = member_of("Member", &founder, 1);
        let group_id = founder.group_id;
        let founder_id = founder.terminal_id;
        let member_id = member.terminal_id;

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(founder, cx);
                store.save(member, cx);
                store.set_group_overview(group_id, true, cx);
                store.rename_group(group_id, "Pair".into(), cx);
                store.delete(founder_id, cx);
            });
        });
        reload_store(cx);

        cx.update(|cx| {
            let store = TerminalThreadMetadataStore::global(cx);
            let store = store.read(cx);
            let group = store.group(group_id).expect("group outlives its founder");
            assert!(group.overview_enabled);
            assert_eq!(group.custom_title.as_deref(), Some("Pair"));
            assert_eq!(
                store.group_display_title(group_id).as_deref(),
                Some("Pair")
            );
            assert_eq!(store.entry(member_id).unwrap().custom_title, None);
        });

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.delete(member_id, cx);
            });
        });
        reload_store(cx);
        cx.update(|cx| {
            let store = TerminalThreadMetadataStore::global(cx);
            assert!(store.read(cx).group(group_id).is_none());
        });
    }

    #[gpui::test]
    async fn test_group_title_uses_oldest_member(cx: &mut TestAppContext) {
        init_test(cx);
        let founder = metadata(
            "Founder",
            WorktreePaths::from_folder_paths(&PathList::default()),
        );
        let mut member = member_of("Member", &founder, 1);
        member.created_at = founder.created_at + chrono::Duration::seconds(1);
        // Swapping the main slot must not change the row title.
        let mut founder = founder;
        founder.position = 2;
        let group_id = founder.group_id;

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(founder, cx);
                store.save(member, cx);
            });
            let store = TerminalThreadMetadataStore::global(cx);
            assert_eq!(
                store.read(cx).group_display_title(group_id).as_deref(),
                Some("Founder")
            );
        });
    }

    #[test]
    fn test_migration_converts_sub_terminals_to_groups() {
        use db::sqlez::connection::Connection;

        let connection = Connection::open_memory(Some("terminal_group_migration"));
        let migrations = TerminalThreadMetadataDb::MIGRATIONS;
        let old_migration_count = 5;
        connection
            .migrate(
                TerminalThreadMetadataDb::NAME,
                &migrations[..old_migration_count],
                &mut |_, _, _| false,
            )
            .unwrap();

        let parent = TerminalId::new();
        let older_child = TerminalId::new();
        let newer_child = TerminalId::new();
        let orphan = TerminalId::new();
        let lone = TerminalId::new();
        let missing_parent = TerminalId::new();
        let rows = [
            (parent, "Parent", Some("Custom"), "2026-01-01T00:00:00+00:00", None, true),
            (newer_child, "Newer", None, "2026-01-03T00:00:00+00:00", Some(parent), false),
            (older_child, "Older", None, "2026-01-02T00:00:00+00:00", Some(parent), false),
            (orphan, "Orphan", None, "2026-01-04T00:00:00+00:00", Some(missing_parent), false),
            (lone, "Lone", None, "2026-01-05T00:00:00+00:00", None, true),
        ];
        for (terminal_id, title, custom_title, created_at, parent_id, overview) in rows {
            let custom_title = custom_title.map_or("NULL".to_string(), |title| format!("'{title}'"));
            let parent_id = parent_id.map_or("NULL".to_string(), |parent_id| {
                format!("'{}'", parent_id.to_key_string())
            });
            connection
                .exec(&format!(
                    "INSERT INTO sidebar_terminal_threads(terminal_id, title, custom_title, \
                     created_at, parent_terminal_id, overview_enabled) \
                     VALUES ('{}', '{title}', {custom_title}, '{created_at}', {parent_id}, {})",
                    terminal_id.to_key_string(),
                    overview as i32,
                ))
                .unwrap()()
            .unwrap();
        }

        connection
            .migrate(TerminalThreadMetadataDb::NAME, migrations, &mut |_, _, _| {
                false
            })
            .unwrap();

        let terminals = connection
            .select::<TerminalThreadMetadata>(LIST_TERMINALS_QUERY)
            .unwrap()()
        .unwrap();
        let group_and_position = |terminal_id: TerminalId| {
            let terminal = terminals
                .iter()
                .find(|terminal| terminal.terminal_id == terminal_id)
                .expect("migrated terminal should exist");
            (terminal.group_id, terminal.position)
        };
        assert_eq!(group_and_position(parent), (parent, 0));
        assert_eq!(group_and_position(older_child), (parent, 1));
        assert_eq!(group_and_position(newer_child), (parent, 2));
        assert_eq!(group_and_position(orphan), (orphan, 0));
        assert_eq!(group_and_position(lone), (lone, 0));

        let groups = connection
            .select::<TerminalGroup>(LIST_GROUPS_QUERY)
            .unwrap()()
        .unwrap();
        assert_eq!(
            groups,
            vec![TerminalGroup {
                group_id: parent,
                overview_enabled: true,
                custom_title: Some("Custom".into()),
            }]
        );

        let columns = connection
            .select::<String>("SELECT name FROM pragma_table_info('sidebar_terminal_threads')")
            .unwrap()()
        .unwrap();
        assert!(!columns.iter().any(|column| column == "parent_terminal_id"));
        assert!(!columns.iter().any(|column| column == "overview_enabled"));
    }

    #[gpui::test]
    async fn test_change_worktree_paths_reindexes_terminal_metadata(cx: &mut TestAppContext) {
        init_test(cx);

        let old_main_paths = PathList::new(&[Path::new("/repo")]);
        let old_folder_paths = PathList::new(&[Path::new("/repo-feature")]);
        let new_main_path = Path::new("/repo");
        let new_folder_path = Path::new("/repo-feature-renamed");
        let new_folder_paths = PathList::new(&[new_folder_path]);
        let metadata = metadata(
            "Dev Server",
            WorktreePaths::from_path_lists(old_main_paths.clone(), old_folder_paths.clone())
                .unwrap(),
        );
        let terminal_id = metadata.terminal_id;

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(metadata, cx);
            });
        });

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.change_worktree_paths(
                    &old_folder_paths,
                    None,
                    |paths| {
                        paths.add_path(new_main_path, new_folder_path);
                        paths.remove_folder_path(Path::new("/repo-feature"));
                    },
                    cx,
                );
            });
        });

        cx.update(|cx| {
            let store = TerminalThreadMetadataStore::global(cx);
            let store = store.read(cx);
            assert!(
                store
                    .entries_for_path(&old_folder_paths, None)
                    .next()
                    .is_none()
            );
            assert_eq!(
                store
                    .entries_for_path(&new_folder_paths, None)
                    .map(|entry| entry.terminal_id)
                    .collect::<Vec<_>>(),
                vec![terminal_id]
            );
            assert_eq!(
                store
                    .entry(terminal_id)
                    .unwrap()
                    .main_worktree_paths()
                    .paths(),
                old_main_paths.paths()
            );
        });
    }
}
